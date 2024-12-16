use crate::{
    brokers::broker::{Broker, BrokerConfig},
    error::BroccoliError,
};

type RedisPool = bb8_redis::bb8::Pool<bb8_redis::RedisConnectionManager>;
type RedisConnection<'a> = bb8_redis::bb8::PooledConnection<'a, bb8_redis::RedisConnectionManager>;

#[derive(Default)]
pub struct RedisBroker {
    pub(crate) redis_pool: Option<RedisPool>,
    pub(crate) connected: bool,
    pub(crate) config: Option<BrokerConfig>,
}

pub(crate) async fn get_redis_connection(
    redis_pool: &RedisPool,
) -> Result<RedisConnection, BroccoliError> {
    let mut redis_conn_sleep = std::time::Duration::from_secs(1);

    #[allow(unused_assignments)]
    let mut opt_redis_connection = None;

    loop {
        let borrowed_redis_connection = match redis_pool.get().await {
            Ok(redis_connection) => Some(redis_connection),
            Err(err) => {
                BroccoliError::Broker(format!("Failed to get redis connection: {:?}", err));
                None
            }
        };

        if borrowed_redis_connection.is_some() {
            opt_redis_connection = borrowed_redis_connection;
            break;
        }

        tokio::time::sleep(redis_conn_sleep).await;
        redis_conn_sleep = std::cmp::min(redis_conn_sleep * 2, std::time::Duration::from_secs(300));
    }

    let redis_connection =
        opt_redis_connection.expect("Failed to get redis connection outside of loop");

    Ok(redis_connection)
}

#[async_trait::async_trait]
impl Broker for RedisBroker {
    async fn connect(&mut self, broker_url: &str) -> Result<(), BroccoliError> {
        let redis_manager = bb8_redis::RedisConnectionManager::new(broker_url).map_err(|e| {
            BroccoliError::Broker(format!("Failed to create redis manager: {:?}", e))
        })?;

        let redis_pool = bb8_redis::bb8::Pool::builder()
            .max_size(
                self.config
                    .as_ref()
                    .map(|config| config.pool_connections.unwrap_or(10))
                    .unwrap_or(10)
                    .into(),
            )
            .connection_timeout(std::time::Duration::from_secs(2))
            .build(redis_manager)
            .await
            .expect("Failed to create redis pool");

        self.redis_pool = Some(redis_pool);
        self.connected = true;
        Ok(())
    }
    async fn publish(&self, queue_name: &str, message: String) -> Result<(), BroccoliError> {
        if let Some(redis_pool) = &self.redis_pool {
            let mut redis_connection = get_redis_connection(redis_pool).await?;

            let _ = redis::cmd("LPUSH")
                .arg(queue_name)
                .arg(&message.to_string())
                .query_async::<String>(&mut *redis_connection)
                .await
                .map_err(|e| {
                    BroccoliError::Publish(format!("Failed to publish message: {:?}", e))
                })?;
        } else {
            return Err(BroccoliError::Broker(
                "Redis pool is not initialized".to_string(),
            ));
        }
        Ok(())
    }

    async fn try_consume(&self, queue_name: &str) -> Result<Option<String>, BroccoliError> {
        if let Some(redis_pool) = &self.redis_pool {
            let mut redis_connection = get_redis_connection(redis_pool).await?;

            let payload: String = redis::cmd("brpoplpush")
                .arg(queue_name)
                .arg(format!("{}_processing", queue_name))
                .arg(1)
                .query_async(&mut *redis_connection)
                .await
                .map_err(|e| {
                    BroccoliError::Consume(format!("Failed to consume message: {:?}", e))
                })?;

            Ok(Some(payload))
        } else {
            Err(BroccoliError::Broker(
                "Redis pool is not initialized".to_string(),
            ))
        }
    }

    async fn consume(&self, queue_name: &str) -> Result<String, BroccoliError> {
        if let Some(redis_pool) = &self.redis_pool {
            let mut redis_connection = get_redis_connection(redis_pool).await?;
            let mut broken_pipe_sleep = std::time::Duration::from_secs(10);
            let mut message: Option<String> = None;

            while message.is_none() {
                let payload_result: Result<Vec<String>, redis::RedisError> =
                    redis::cmd("brpoplpush")
                        .arg(queue_name)
                        .arg(format!("{}_processing", queue_name))
                        .arg(1)
                        .query_async(&mut *redis_connection)
                        .await;

                let serialized_message = if let Ok(payload) = payload_result {
                    broken_pipe_sleep = std::time::Duration::from_secs(10);

                    if payload.is_empty() {
                        continue;
                    }

                    if let Some(first_element) = payload.first() {
                        first_element.clone()
                    } else {
                        return Err(BroccoliError::Consume(
                            "Failed to consume message: Payload is empty".to_string(),
                        ));
                    }
                } else {
                    if payload_result.is_err_and(|err| err.is_io_error()) {
                        tokio::time::sleep(broken_pipe_sleep).await;
                        broken_pipe_sleep = std::cmp::min(
                            broken_pipe_sleep * 2,
                            std::time::Duration::from_secs(300),
                        );
                    }

                    continue;
                };

                message = Some(serialized_message);
            }

            Ok(message.expect("Should have a message to exit loop"))
        } else {
            Err(BroccoliError::Broker(
                "Redis pool is not initialized".to_string(),
            ))
        }
    }

    async fn acknowledge(&self, queue_name: &str, message: String) -> Result<(), BroccoliError> {
        if let Some(redis_pool) = &self.redis_pool {
            let mut redis_connection = get_redis_connection(redis_pool).await?;

            let _ = redis::cmd("LREM")
                .arg(format!("{}_processing", queue_name))
                .arg(1)
                .arg(message)
                .query_async::<String>(&mut *redis_connection)
                .await;
        }
        Ok(())
    }

    async fn reject(&self, queue_name: &str, message: String) -> Result<(), BroccoliError> {
        if let Some(redis_pool) = &self.redis_pool {
            let mut redis_connection = get_redis_connection(redis_pool).await?;

            let attempts = RedisBroker::extract_message_attempts(&message) + 1;

            let _ = redis::cmd("LREM")
                .arg(format!("{}_processing", queue_name))
                .arg(1)
                .arg(message.clone())
                .query_async::<String>(&mut *redis_connection)
                .await
                .map_err(|e| {
                    BroccoliError::Publish(format!("Failed to publish message: {:?}", e))
                })?;

            if !attempts
                < self
                    .config
                    .as_ref()
                    .map(|config| config.retry_attempts.unwrap_or(3))
                    .unwrap_or(3)
            {
                redis::cmd("lpush")
                    .arg(format!("{}_failed", queue_name))
                    .arg(message)
                    .query_async::<String>(&mut *redis_connection)
                    .await
                    .map_err(|err| {
                        BroccoliError::Publish(format!("Failed to push to failed queue: {:?}", err))
                    })?;

                return Err(BroccoliError::Broker("Message failed 3 times".to_string()));
            }

            let new_message = RedisBroker::update_attempts(message, attempts);

            let _ = redis::cmd("LPUSH")
                .arg(queue_name)
                .arg(new_message)
                .query_async::<String>(&mut *redis_connection)
                .await
                .map_err(|e| {
                    BroccoliError::Publish(format!("Failed to publish message: {:?}", e))
                })?;
        } else {
            return Err(BroccoliError::Broker(
                "Redis pool is not initialized".to_string(),
            ));
        }

        Ok(())
    }
}
