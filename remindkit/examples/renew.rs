//! Live check of background renewal: reads a saved session (JSON) on stdin,
//! runs `validate_or_renew` without a password and prints how it went.
//! Prints no secrets and saves nothing.
//!
//!     secret-tool lookup service io.github.mozartdiniz.iCloud username web-session | cargo run --example renew

use std::io::Read;

use remindkit::ICloudClient;

#[tokio::main]
async fn main() {
    let mut saved = String::new();
    std::io::stdin().read_to_string(&mut saved).expect("read stdin");
    let mut client = ICloudClient::restore(saved.trim()).expect("a saved session");
    let first = client.validate().await;
    println!("validate as given: {:?}", first.map(|_| "valid"));
    let res = client.validate_or_renew(None).await;
    println!("validate_or_renew(no password): {res:?}");
    if res.is_ok() {
        println!("validate after: {:?}", client.validate().await.map(|_| "valid"));
    }
}
