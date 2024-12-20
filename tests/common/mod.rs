use broccoli_queue::queue::BroccoliQueue;

pub async fn setup_queue() -> BroccoliQueue {
    BroccoliQueue::builder("redis://localhost:6380")
        .pool_connections(5)
        .build()
        .await
        .expect("Queue setup failed. Are you sure Redis is running on localhost:6380?")
}

pub async fn setup_queue_with_url(
    url: &str,
) -> Result<BroccoliQueue, broccoli_queue::error::BroccoliError> {
    BroccoliQueue::builder(url)
        .pool_connections(5)
        .build()
        .await
}
