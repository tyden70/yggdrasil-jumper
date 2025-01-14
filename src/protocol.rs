use super::*;

/* Protocol is stupid simple:
 *
 *  0. Select availible external addresses.
 *  1. Send `header` to peer.
 *  2. Receive `header` from peer.
 *  3. Check if version and address ranges are intersected.
 *  4. Send self external address (ipv6 is preferred).
 *  5. Receive peer's external address.
 *  6. Validate external addresses.
 *  7. Close socket.
 *  8. Try NAT traversal.
 *
 * All comminucation is in length-delimited JSON packets using `tokio_util::codec::LengthDelimitedCodec`.
*/

// Align connection time with session's uptime for firewall traversal effect
pub const ALIGN_UPTIME_TIMEOUT: f64 = 20.0;

const VERSION: &str = "yggdrasil-jumper-tcp-v0";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: String,
    ipv4: bool,
    ipv6: bool,
}

#[instrument(parent = None, name = "Session ", skip_all, fields(peer = %address))]
pub async fn try_session(
    config: Config,
    state: State,
    socket: TcpStream,
    address: SocketAddrV6,
) -> Result<(), ()> {
    let (mut sink, mut stream) = Framed::new(socket, LengthDelimitedCodec::new()).split();

    // 0. Select availible external addresses.
    let (ipv6, ipv4) = {
        let addresses = state.watch_external.borrow();
        (
            if config.allow_ipv6 {
                addresses.iter().map(|a| a.external).find(|a| a.is_ipv6())
            } else {
                None
            },
            if config.allow_ipv4 {
                addresses.iter().map(|a| a.external).find(|a| a.is_ipv4())
            } else {
                None
            },
        )
    };

    // 1. Send `header` to peer.
    sink.send(bytes::Bytes::from(
        serde_json::to_vec(&protocol::Header {
            version: protocol::VERSION.to_string(),
            ipv4: ipv4.is_some(),
            ipv6: ipv6.is_some(),
        })
        .expect("Protocol request header can't be serialized"),
    ))
    .await
    .map_err(map_info!("Failed to send protocol header to peer"))?;

    // 2. Receive `header` from peer.
    let remote_header: protocol::Header = serde_json::from_reader(std::io::Cursor::new(
        stream
            .next()
            .await
            .ok_or_else(|| info!("Failed to receive header: Connection closed"))?
            .map_err(map_info!("Failed to receive incoming header"))?,
    ))
    .map_err(map_info!("Failed to prarse incoming header"))?;

    // 3. Check if version and address ranges are intersected.
    if remote_header.version != protocol::VERSION {
        return Err(info!(
            "Protocol version missmatch: expected: {:?}, received: {:?}",
            remote_header.version,
            protocol::VERSION
        ));
    }

    let external = (|| {
        // ipv6 is preferred
        if let (true, Some(a)) = (remote_header.ipv6, ipv6) {
            return Ok(a);
        }
        if let (true, Some(a)) = (remote_header.ipv4, ipv4) {
            return Ok(a);
        }
        warn!(
            "Have no address to share with peer (self: v4={}, v6={}; remote: v4={}, v6={})",
            ipv4.is_some(),
            ipv6.is_some(),
            remote_header.ipv4,
            remote_header.ipv6
        );
        Err(())
    })()?;

    // 4. Send self external address.
    sink.send(
        serde_json::to_vec(&external)
            .expect("Self external addresses can't be serialized")
            .into(),
    )
    .await
    .map_err(map_info!("Failed to send self external addresses to peer"))?;

    // 5. Receive peer's external address.
    let remote_external: SocketAddr = serde_json::from_slice(
        &stream
            .next()
            .await
            .ok_or_else(|| info!("Failed to receive peer's external addresses: Connection closed"))?
            .map_err(map_info!("Failed to receive peer's external addresses"))?,
    )
    .map_err(map_info!("Failed to prarse peer's external addresses"))?;

    // 6. Validate external addresses.
    match (external, remote_external) {
        (SocketAddr::V6(_), SocketAddr::V6(_)) => (),
        (SocketAddr::V4(_), SocketAddr::V4(_)) => (),
        _ => {
            info!("External addresses have incompatible ranges: self {external:?}, remote {remote_external:?}");
            return Err(());
        }
    }

    // 7. Close socket.
    drop(sink.reunite(stream));

    // 8. Try NAT traversal.
    let local = state
        .watch_external
        .borrow()
        .iter()
        .find(|addr| addr.external == external)
        .ok_or_else(|| info!("Expected external address unavailible: {external}"))?
        .local;
    let remote = remote_external;

    if let Ok(socket) = internet::traverse(
        config.clone(),
        state.clone(),
        local.port(),
        remote,
        Some(*address.ip()),
    )
    .await
    .map_err(map_info!("NAT traversal failed"))
    {
        return bridge::run_bridge(config, state, remote, Some(*address.ip()), socket).await;
    }
    Err(())
}
