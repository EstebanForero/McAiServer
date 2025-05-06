use std::io;

use tokio::{io::AsyncReadExt, net::TcpSocket};

#[tokio::main]
async fn main() -> io::Result<()> {
    let addr = "127.0.0.1:8080".parse().unwrap();

    let socket = TcpSocket::new_v4()?;
    let mut stream = socket.connect(addr).await?;

    let mut buffer = [0; 1500];

    while stream.read(&mut buffer).await.is_ok() {
        println!("{buffer:?}");
    }

    Ok(())
}
