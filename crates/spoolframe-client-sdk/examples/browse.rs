//! Find a server on the LAN, pin it, sign in and print the home screen:
//! the SDK end to end on a real network.
//!
//! ```text
//! SPOOLFRAME_USER=alice SPOOLFRAME_PASSWORD=… \
//!   cargo run -p spoolframe-client-sdk --example browse
//! ```
//!
//! Storage is in memory, so every run is a new device.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Item};
use spoolframe_client_sdk::{Client, Device, MemoryStorage};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let user = std::env::var("SPOOLFRAME_USER")?;
    let password = std::env::var("SPOOLFRAME_PASSWORD")?;
    let device = Device {
        name: "browse example".into(),
        client_name: "spoolframe-sdk-browse".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        platform: spoolframe_client_sdk::types::Platform::Cli,
        capabilities: ClientCapabilities::default(),
    };
    let client = Client::open(Arc::new(MemoryStorage::default()), device).await?;
    println!("this device: {}", client.id());

    let server = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(s) = client.servers().await?.into_iter().next() {
                return anyhow::Ok(s);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("no server heard on the LAN within 10 s"))??;
    println!(
        "heard {:?} ({}) at {:?}",
        server.name, server.endpoint_id, server.addrs
    );

    let pinned = client.connect(&server.endpoint_id).await?;
    println!("pinned: {}", pinned.pinned);
    let session = client
        .sign_in(&server.endpoint_id, &user, &password)
        .await?;
    println!("signed in as {}", session.user().display_name);

    for library in session.libraries().await? {
        let page = session.items(&library.id, 0, 1).await?;
        println!("library {:?}: {} items", library.title, page.total);
    }
    let home = session.home().await?;
    let titles = |items: &[Item]| -> Vec<String> {
        items.iter().take(5).map(|i| i.id().to_string()).collect()
    };
    println!("continue watching: {:?}", titles(&home.continue_watching));
    println!("next up: {:?}", titles(&home.next_up));
    println!("recently added: {:?}", titles(&home.recently_added));
    session.sign_out().await?;
    client.close().await;
    Ok(())
}
