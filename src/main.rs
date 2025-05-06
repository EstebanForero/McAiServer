use std::io;

use tokio::{io::AsyncReadExt, net::TcpSocket};

#[tokio::main]
async fn main() -> io::Result<()> {
    let addr = "170.20.10.2:8080".parse().unwrap();

    let socket = TcpSocket::new_v4()?;
    let mut stream = socket.connect(addr).await?;

    let mut buffer = [0; 1500];

    let mut offset = 0;

    while stream.read(&mut buffer).await.is_ok() {
        let audio_value = i16::from_le_bytes([buffer[offset], buffer[offset + 1]]);
        println!("{}", audio_value);
        offset += 8;
    }

    Ok(())
}
