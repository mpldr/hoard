//! Leer y escribir tramas sobre el socket.
//!
//! El formato (cabecera `u32` big-endian + JSON) y su tope viven en
//! [`hoard_core::ipc`], que es serde-puro; aquí está lo único que necesita
//! runtime: mover bytes. Esa división es la del kernel leaf de la ADR — el
//! contrato no puede depender de `tokio`, y el transporte sí.

use anyhow::{Context, Result};
use hoard_core::ipc::{decode_frame, encode_frame, frame_len, HEADER_BYTES};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Escribe una trama completa. Una sola llamada a `write_all`: dos tramas
/// escritas desde tasks distintas no pueden entrelazarse a medias.
pub async fn write_frame<W, T>(writer: &mut W, message: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = encode_frame(message).context("encoding a frame")?;
    writer.write_all(&bytes).await.context("writing a frame")?;
    writer.flush().await.context("flushing a frame")?;
    Ok(())
}

/// Lee una trama. `Ok(None)` = el otro extremo cerró limpiamente, que para un
/// cliente que se va es lo normal y no un error que loguear.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut header = [0u8; HEADER_BYTES];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(anyhow::Error::new(err).context("reading a frame header")),
    }
    // El tope se comprueba antes de reservar: un prefijo de 4 GiB no puede
    // convertirse en una reserva de 4 GiB por mucho que el socket sea local.
    let len = frame_len(header).context("validating a frame header")?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .context("reading a frame body")?;
    Ok(Some(decode_frame(&body).context("decoding a frame")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_core::ipc::{ClientFrame, Hello, Request, PROTOCOL_VERSION};

    #[tokio::test]
    async fn frames_survive_a_round_trip_over_a_pipe() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let hello = ClientFrame::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            client: "test".into(),
        });
        write_frame(&mut client, &hello).await.unwrap();
        write_frame(
            &mut client,
            &ClientFrame::Request {
                id: 1,
                request: Request::Ping,
            },
        )
        .await
        .unwrap();
        drop(client);

        let first: ClientFrame = read_frame(&mut server).await.unwrap().unwrap();
        assert!(matches!(first, ClientFrame::Hello(_)));
        let second: ClientFrame = read_frame(&mut server).await.unwrap().unwrap();
        assert!(matches!(
            second,
            ClientFrame::Request {
                id: 1,
                request: Request::Ping
            }
        ));
        // Cierre limpio, no error.
        let end: Option<ClientFrame> = read_frame(&mut server).await.unwrap();
        assert!(end.is_none());
    }

    /// Una cabecera que promete más de lo permitido se rechaza sin reservar el
    /// buffer que promete.
    #[tokio::test]
    async fn an_oversized_header_is_rejected() {
        let (mut client, mut server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            let _ = client.write_all(&u32::MAX.to_be_bytes()).await;
            // Sin cuerpo: si el lector intentara leerlo, se quedaría colgado en
            // vez de fallar.
            futures_keepalive(client).await;
        });
        let err = read_frame::<_, ClientFrame>(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("frame header"), "{err}");
    }

    /// Mantiene el otro extremo abierto para que el test de arriba falle por el
    /// tope y no por EOF.
    async fn futures_keepalive(stream: tokio::io::DuplexStream) {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(stream);
    }
}
