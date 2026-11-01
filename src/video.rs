//! Capture the documented VIC UDP stream, accepting only complete frames.
//!
//! The packet format and palette follow the Ultimate documentation and its
//! `python/grab.py` example. This controls the device's single video stream;
//! starting video also stops a running debug stream. Previous streams and their
//! destinations are not restored.

use std::{
    collections::VecDeque,
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::Method;
use tokio::{net::UdpSocket, sync::OwnedMutexGuard};

use crate::device::Device;

const WIDTH: usize = 384;
const MAX_HEIGHT: usize = 272;
const PACKET_ROWS: usize = 4;
const PAYLOAD_BYTES: usize = WIDTH * PACKET_ROWS / 2;
const PACKET_BYTES: usize = 12 + PAYLOAD_BYTES;
const MAX_PENDING_FRAMES: usize = 4;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

// RGB palette from https://github.com/GideonZ/1541ultimate/blob/master/python/grab.py.
// The stream contains color indices, so a PNG's displayed colors depend on this palette.
const PALETTE: [u8; 48] = [
    0x00, 0x00, 0x00, 0xEF, 0xEF, 0xEF, 0x8D, 0x2F, 0x34, 0x6A, 0xD4, 0xCD, 0x98, 0x35, 0xA4, 0x4C,
    0xB4, 0x42, 0x2C, 0x29, 0xB1, 0xEF, 0xEF, 0x5D, 0x98, 0x4E, 0x20, 0x5B, 0x38, 0x00, 0xD1, 0x67,
    0x6D, 0x4A, 0x4A, 0x4A, 0x7B, 0x7B, 0x7B, 0x9F, 0xEF, 0x93, 0x6D, 0x6A, 0xEF, 0xB2, 0xB2, 0xB2,
];

#[derive(Debug)]
struct Frame {
    number: u16,
    height: usize,
    pixels: Vec<u8>,
}

impl Frame {
    fn png(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, WIDTH as u32, self.height as u32);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(PALETTE.to_vec());
            let mut writer = encoder
                .write_header()
                .context("encode screenshot PNG header")?;
            writer
                .write_image_data(&self.pixels)
                .context("encode screenshot PNG pixels")?;
            writer.finish().context("finish screenshot PNG")?;
        }
        Ok(bytes)
    }
}

struct PendingFrame {
    number: u16,
    height: Option<usize>,
    // Fixed-size backing storage avoids allocations based on untrusted headers.
    packed: Vec<u8>,
    present: [bool; MAX_HEIGHT / PACKET_ROWS],
    final_flags: [bool; MAX_HEIGHT / PACKET_ROWS],
    bad: bool,
}

impl PendingFrame {
    fn new(number: u16) -> Self {
        Self {
            number,
            height: None,
            packed: vec![0; WIDTH * MAX_HEIGHT / 2],
            present: [false; MAX_HEIGHT / PACKET_ROWS],
            final_flags: [false; MAX_HEIGHT / PACKET_ROWS],
            bad: false,
        }
    }
}

#[derive(Default)]
struct VideoAssembler {
    pending: VecDeque<PendingFrame>,
    last_completed: Option<u16>,
    packets: usize,
    invalid_packets: usize,
}

impl VideoAssembler {
    fn feed(&mut self, packet: &[u8]) -> Option<Frame> {
        self.packets = self.packets.saturating_add(1);
        if packet.len() != PACKET_BYTES {
            self.invalid_packets = self.invalid_packets.saturating_add(1);
            return None;
        }
        let word = |offset| u16::from_le_bytes([packet[offset], packet[offset + 1]]);
        let number = word(2);
        let raw_line = word(4);
        let last = raw_line & 0x8000 != 0;
        let line = usize::from(raw_line & 0x7fff);
        if word(6) as usize != WIDTH
            || packet[8] != PACKET_ROWS as u8
            || packet[9] != 4
            || word(10) != 0
            || line % PACKET_ROWS != 0
            || line > MAX_HEIGHT - PACKET_ROWS
            || (last && !matches!(line + PACKET_ROWS, 240 | 272))
        {
            self.invalid_packets = self.invalid_packets.saturating_add(1);
            return None;
        }
        if self.last_completed == Some(number) {
            return None;
        }
        let index = match self.pending.iter().position(|frame| frame.number == number) {
            Some(index) => index,
            None => {
                if self.pending.len() == MAX_PENDING_FRAMES {
                    self.pending.pop_front();
                }
                self.pending.push_back(PendingFrame::new(number));
                self.pending.len() - 1
            }
        };
        let frame = &mut self.pending[index];
        let slot = line / PACKET_ROWS;
        let offset = line * WIDTH / 2;
        if frame.present[slot]
            && (frame.packed[offset..offset + PAYLOAD_BYTES] != packet[12..]
                || frame.final_flags[slot] != last)
        {
            frame.bad = true;
        }
        frame.packed[offset..offset + PAYLOAD_BYTES].copy_from_slice(&packet[12..]);
        frame.present[slot] = true;
        frame.final_flags[slot] = last;
        if last {
            let height = line + PACKET_ROWS;
            if frame.height.is_some_and(|old| old != height) {
                frame.bad = true;
            }
            frame.height = Some(height);
        }
        let height = frame.height?;
        let slots = height / PACKET_ROWS;
        // Extraneous rows make a purported shorter frame inconsistent.
        if frame.bad
            || frame.present[slots..].iter().any(|present| *present)
            || !frame.present[..slots].iter().all(|present| *present)
        {
            return None;
        }
        let packed = self.pending.remove(index).expect("pending frame exists");
        let mut pixels = Vec::with_capacity(WIDTH * height);
        for byte in &packed.packed[..WIDTH * height / 2] {
            pixels.extend([byte & 0x0f, byte >> 4]);
        }
        self.last_completed = Some(number);
        Some(Frame {
            number,
            height,
            pixels,
        })
    }
}

/// A capture owns its caller's device-operation lock until stream cleanup ends,
/// including when the MCP request is cancelled and this guard is dropped.
struct StreamCleanup {
    device: Device,
    operation_guard: Option<Arc<OwnedMutexGuard<()>>>,
    armed: bool,
}

impl StreamCleanup {
    async fn finish(&mut self) -> Result<()> {
        let result = stop_stream(&self.device).await;
        self.armed = false;
        result
    }
}

impl Drop for StreamCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let device = self.device.clone();
        let operation_guard = self.operation_guard.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // Keep the lock in this task even if the original request has gone away.
                let _operation_guard = operation_guard;
                if let Err(error) = stop_stream(&device).await {
                    tracing::warn!("video cleanup after cancellation failed: {error:#}");
                }
            });
        } else {
            tracing::warn!("video stream cleanup unavailable: async runtime has shut down");
        }
    }
}

async fn stop_stream(device: &Device) -> Result<()> {
    tokio::time::timeout(
        CLEANUP_TIMEOUT,
        device.json(Method::PUT, "streams/video:stop", &[], None),
    )
    .await
    .context("timed out stopping the video stream; it may still be running")?
    .context("failed to stop the video stream; it may still be running")?;
    Ok(())
}

/// Locks onto the first sender of a valid video packet and rejects others.
///
/// The device may send video from a different address than the one used for
/// HTTP (another interface, or another DNS record), so the sender is learned
/// from the stream itself rather than from the configured host.
#[derive(Default)]
struct SenderLock {
    sender: Option<IpAddr>,
    foreign_packets: usize,
}

impl SenderLock {
    fn accepts(&mut self, peer: IpAddr) -> bool {
        match self.sender {
            Some(sender) if sender != peer => {
                self.foreign_packets = self.foreign_packets.saturating_add(1);
                false
            }
            _ => true,
        }
    }

    fn observe_valid(&mut self, peer: IpAddr) {
        self.sender.get_or_insert(peer);
    }
}

async fn receive_frame(socket: &UdpSocket, duration: Duration) -> Result<Frame> {
    let mut assembler = VideoAssembler::default();
    let mut lock = SenderLock::default();
    // Receive any UDP datagram in a fixed-size buffer. A smaller buffer causes
    // WSAEMSGSIZE on Windows before sender filtering or packet validation runs.
    // The assembler still accepts only exact 780-byte video packets.
    let mut buffer = [0u8; 65536];
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        match tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await {
            Ok(received) => {
                let (length, peer) = received.context("receive VIC video datagram")?;
                if !lock.accepts(peer.ip()) {
                    continue;
                }
                let invalid_before = assembler.invalid_packets;
                let frame = assembler.feed(&buffer[..length]);
                if assembler.invalid_packets == invalid_before {
                    lock.observe_valid(peer.ip());
                }
                if let Some(frame) = frame {
                    return Ok(frame);
                }
            }
            Err(_) if assembler.packets == 0 => {
                bail!(
                    "no video packets received before timeout; check capture_ip, device stream support, and the local UDP firewall"
                )
            }
            Err(_) => bail!(
                "no complete video frame received before timeout ({} packets, {} invalid, {} incomplete frames, {} from other senders)",
                assembler.packets,
                assembler.invalid_packets,
                assembler.pending.len(),
                lock.foreign_packets
            ),
        }
    }
}

/// Capture one complete PAL/NTSC frame as an indexed PNG.
///
/// `capture_ip` is a local interface reachable from the C64U; absent means infer
/// the local IPv4 route. Port zero chooses a free port. The operation takes over
/// the video destination and stops video afterward; it does not restore another
/// application's stream. Cleanup is best effort after cancellation/process exit.
pub async fn capture(
    device: &Device,
    capture_ip: Option<Ipv4Addr>,
    port: u16,
    timeout: Duration,
    operation_guard: Option<Arc<OwnedMutexGuard<()>>>,
) -> Result<Vec<u8>> {
    ensure!(
        !timeout.is_zero() && timeout <= Duration::from_secs(60),
        "capture timeout must be greater than zero and at most 60 seconds"
    );
    let deadline = tokio::time::Instant::now() + timeout;
    let base = device
        .base_url()
        .context("no C64U configured; set C64U_HOST or --host")?;
    let host = base.host_str().context("configured C64U URL has no host")?;
    let target_port = base.port_or_known_default().unwrap_or(80);
    let socket = tokio::time::timeout_at(deadline, async {
        let addresses = tokio::net::lookup_host((host, target_port))
            .await
            .context("resolve C64U IPv4 address")?;
        let target = addresses
            .filter_map(|address| match address.ip() {
                IpAddr::V4(ip) => Some(ip),
                IpAddr::V6(_) => None,
            })
            .next()
            .context("video capture requires an IPv4 C64U address")?;
        let local = match capture_ip {
            Some(ip) => {
                ensure!(
                    !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast(),
                    "capture_ip must be a concrete local unicast IPv4 address"
                );
                ip
            }
            None => {
                let route_socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
                // UDP connect selects a route without transmitting a datagram.
                route_socket
                    .connect((target, target_port))
                    .await
                    .context("infer local video capture interface")?;
                match route_socket.local_addr()?.ip() {
                    IpAddr::V4(ip) => ip,
                    IpAddr::V6(_) => bail!("could not infer an IPv4 capture interface"),
                }
            }
        };
        let socket = UdpSocket::bind((local, port))
            .await
            .context("bind local video capture UDP socket")?;
        Ok::<_, anyhow::Error>(socket)
    })
    .await
    .context("timed out preparing video capture")??;
    let destination = socket.local_addr()?.to_string();
    let mut cleanup = StreamCleanup {
        device: device.clone(),
        operation_guard,
        armed: true,
    };
    // Arm before sending start: the device can start even if its response is lost.
    let result = async {
        tokio::time::timeout_at(
            deadline,
            device.json(
                Method::PUT,
                "streams/video:start",
                &[("ip".to_owned(), destination)],
                None,
            ),
        )
        .await
        .context("timed out starting video stream")??;
        let frame = receive_frame(
            &socket,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await?;
        tracing::debug!(
            frame = frame.number,
            height = frame.height,
            "captured complete VIC frame"
        );
        frame.png()
    }
    .await;
    let stopped = cleanup.finish().await;
    match (result, stopped) {
        (Ok(png), Ok(())) => Ok(png),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => Err(error.context(format!(
            "video stream cleanup also failed: {cleanup_error:#}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    async fn mock_device() -> (MockServer, Device) {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/streams/video:start"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"errors": []})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let device = Device::new(Some(&server.uri()), None, Duration::from_secs(5)).unwrap();
        (server, device)
    }

    async fn started_destination(server: &MockServer) -> std::net::SocketAddr {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let requests = server.received_requests().await.unwrap();
                if let Some(request) = requests
                    .iter()
                    .find(|request| request.url.path().ends_with("video:start"))
                {
                    return request
                        .url
                        .query_pairs()
                        .find(|(name, _)| name == "ip")
                        .unwrap()
                        .1
                        .parse()
                        .unwrap();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("stream start request")
    }

    fn packet(row: usize, number: u16, height: usize, color: u8) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PACKET_BYTES);
        bytes.extend((row as u16 / 4).to_le_bytes());
        bytes.extend(number.to_le_bytes());
        bytes.extend(((row as u16) | if row + 4 == height { 0x8000 } else { 0 }).to_le_bytes());
        bytes.extend((WIDTH as u16).to_le_bytes());
        bytes.extend([4, 4, 0, 0]);
        bytes.resize(PACKET_BYTES, color);
        bytes
    }

    fn complete_frame(decoder: &mut VideoAssembler, number: u16, height: usize) -> Frame {
        let mut result = None;
        for row in (0..height).step_by(4) {
            result = decoder.feed(&packet(row, number, height, 0xa2));
        }
        result.expect("complete frame")
    }

    #[test]
    fn pal_and_ntsc_frames_preserve_nibble_order_and_png_pixels() {
        for height in [240, 272] {
            let frame = complete_frame(&mut VideoAssembler::default(), 3, height);
            assert_eq!(frame.pixels, [2, 10].repeat(WIDTH * height / 2));
            let bytes = frame.png().unwrap();
            let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
                .read_info()
                .unwrap();
            assert_eq!(reader.info().width, WIDTH as u32);
            assert_eq!(reader.info().height, height as u32);
            assert_eq!(reader.info().palette.as_deref(), Some(PALETTE.as_slice()));
            let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut decoded).unwrap();
            assert_eq!(&decoded[..info.buffer_size()], frame.pixels);
        }
    }

    #[test]
    fn reverse_order_and_identical_duplicates_are_supported() {
        let mut decoder = VideoAssembler::default();
        let mut result = None;
        for row in (0..272).step_by(4).rev() {
            let bytes = packet(row, 7, 272, 0x12);
            result = decoder.feed(&bytes);
            assert!(decoder.feed(&bytes).is_none());
        }
        assert_eq!(result.unwrap().height, 272);
    }

    #[test]
    fn dropped_rows_and_conflicting_duplicates_never_complete() {
        for conflict in [false, true] {
            let mut decoder = VideoAssembler::default();
            if conflict {
                decoder.feed(&packet(32, 1, 240, 0x99));
            }
            for row in (0..240).step_by(4) {
                if conflict || row != 32 {
                    assert!(decoder.feed(&packet(row, 1, 240, 0xa2)).is_none());
                }
            }
        }
    }

    #[test]
    fn invalid_lengths_headers_and_inconsistent_final_rows_are_rejected() {
        let mut decoder = VideoAssembler::default();
        let good = packet(0, 1, 240, 0x12);
        assert!(decoder.feed(&good[..779]).is_none());
        for (offset, value) in [(6, 0), (8, 5), (9, 8), (10, 1), (4, 1), (5, 0x80)] {
            let mut malformed = good.clone();
            malformed[offset] = value;
            assert!(decoder.feed(&malformed).is_none());
        }
        assert_eq!(decoder.invalid_packets, 7);
        // A frame cannot contain both NTSC and PAL end markers.
        decoder.feed(&packet(268, 1, 272, 0xa2));
        for row in (0..240).step_by(4) {
            assert!(decoder.feed(&packet(row, 1, 240, 0xa2)).is_none());
        }
    }

    #[test]
    fn final_flag_conflict_and_rows_beyond_height_reject_frame() {
        let mut decoder = VideoAssembler::default();
        decoder.feed(&packet(236, 1, 272, 0xa2));
        for row in (0..240).step_by(4) {
            assert!(decoder.feed(&packet(row, 1, 240, 0xa2)).is_none());
        }
        let mut decoder = VideoAssembler::default();
        decoder.feed(&packet(240, 1, 272, 0xa2));
        for row in (0..240).step_by(4) {
            assert!(decoder.feed(&packet(row, 1, 240, 0xa2)).is_none());
        }
    }

    #[test]
    fn frame_number_wrap_and_bounded_pending_storage() {
        let mut decoder = VideoAssembler::default();
        assert_eq!(complete_frame(&mut decoder, u16::MAX, 240).number, u16::MAX);
        assert_eq!(complete_frame(&mut decoder, 0, 240).number, 0);
        for number in 1..100 {
            decoder.feed(&packet(0, number, 240, 0));
            assert!(decoder.pending.len() <= MAX_PENDING_FRAMES);
        }
        assert_eq!(decoder.pending.front().unwrap().number, 96);
    }

    #[tokio::test]
    async fn localhost_receives_complete_reordered_frame() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let destination = receiver.local_addr().unwrap();
        let sender = tokio::spawn(async move {
            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            for row in (0..240).step_by(4).rev() {
                socket
                    .send_to(&packet(row, 9, 240, 0xa2), destination)
                    .await
                    .unwrap();
                // Keep the test independent of OS-specific UDP receive-buffer sizes.
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let frame = receive_frame(&receiver, Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(frame.number, 9);
        assert_eq!(frame.pixels.len(), WIDTH * 240);
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_datagram_is_discarded_before_a_valid_frame() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let destination = receiver.local_addr().unwrap();
        let sender = tokio::spawn(async move {
            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            // Windows reports WSAEMSGSIZE if the receive buffer is smaller than
            // a datagram, instead of returning its truncated contents.
            let mut oversized = packet(0, 17, 240, 0x12);
            oversized.resize(4096, 0xff);
            socket.send_to(&oversized, destination).await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
            for row in (0..240).step_by(4) {
                socket
                    .send_to(&packet(row, 18, 240, 0xa2), destination)
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let frame = receive_frame(&receiver, Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(frame.number, 18);
        assert_eq!(frame.pixels, [2, 10].repeat(WIDTH * 240 / 2));
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn timeouts_distinguish_missing_packets_from_incomplete_frame() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let timeout = Duration::from_millis(25);
        let error = receive_frame(&receiver, timeout).await.unwrap_err();
        assert!(error.to_string().contains("no video packets"));
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        sender
            .send_to(&packet(0, 1, 240, 0), receiver.local_addr().unwrap())
            .await
            .unwrap();
        let error = receive_frame(&receiver, timeout).await.unwrap_err();
        assert!(error.to_string().contains("no complete video frame"));
    }

    #[test]
    fn sender_lock_follows_first_valid_sender() {
        let device = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let other = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
        let mut lock = SenderLock::default();
        // Before any valid packet, any sender is considered, including one
        // that differs from the configured HTTP host.
        assert!(lock.accepts(other));
        assert!(lock.accepts(device));
        lock.observe_valid(device);
        assert!(lock.accepts(device));
        assert!(!lock.accepts(other));
        lock.observe_valid(other);
        assert!(!lock.accepts(other));
        assert_eq!(lock.foreign_packets, 2);
    }

    #[tokio::test]
    async fn capture_binds_before_start_and_stops_after_success() {
        let (server, device) = mock_device().await;
        Mock::given(method("PUT"))
            .and(path("/v1/streams/video:stop"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"errors": []})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let task = tokio::spawn(async move {
            capture(
                &device,
                Some(Ipv4Addr::LOCALHOST),
                0,
                Duration::from_secs(3),
                None,
            )
            .await
        });
        let destination = started_destination(&server).await;
        assert_ne!(destination.port(), 0);
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        for row in (0..240).step_by(4) {
            sender
                .send_to(&packet(row, 1, 240, 0xa2), destination)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let image = task.await.unwrap().unwrap();
        assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
        server.verify().await;
    }

    #[tokio::test]
    async fn capture_stops_after_timeout() {
        let (server, device) = mock_device().await;
        Mock::given(method("PUT"))
            .and(path("/v1/streams/video:stop"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"errors": []})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = capture(
            &device,
            Some(Ipv4Addr::LOCALHOST),
            0,
            Duration::from_millis(200),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("no video packets"));
        server.verify().await;
    }

    #[tokio::test]
    async fn cancellation_stops_stream_and_holds_operation_lock_through_cleanup() {
        let (server, device) = mock_device().await;
        Mock::given(method("PUT"))
            .and(path("/v1/streams/video:stop"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"errors": []}))
                    .set_delay(Duration::from_millis(150)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let operation_guard = Arc::new(lock.clone().lock_owned().await);
        let task = tokio::spawn(async move {
            capture(
                &device,
                Some(Ipv4Addr::LOCALHOST),
                0,
                Duration::from_secs(3),
                Some(operation_guard),
            )
            .await
        });
        started_destination(&server).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            lock.try_lock().is_err(),
            "cleanup must retain the caller's lock"
        );
        let _released = tokio::time::timeout(Duration::from_secs(3), lock.lock())
            .await
            .unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn capture_failure_preserves_primary_and_cleanup_errors_without_retry() {
        let (server, device) = mock_device().await;
        Mock::given(method("PUT"))
            .and(path("/v1/streams/video:stop"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_json(serde_json::json!({"errors": ["stop rejected"]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = capture(
            &device,
            Some(Ipv4Addr::LOCALHOST),
            0,
            Duration::from_millis(100),
            None,
        )
        .await
        .unwrap_err();
        let detail = format!("{error:#}");
        assert!(detail.contains("no video packets"));
        assert!(detail.contains("video stream cleanup also failed"));
        assert!(detail.contains("stop rejected"));
        server.verify().await;
    }

    #[tokio::test]
    async fn cancelling_inflight_stop_keeps_lock_until_bounded_cleanup_finishes() {
        let server = MockServer::start().await;
        let stopping = Arc::new(tokio::sync::Notify::new());
        let signal = stopping.clone();
        Mock::given(method("PUT"))
            .and(path("/v1/streams/video:stop"))
            .respond_with(move |_: &wiremock::Request| {
                signal.notify_one();
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"errors": []}))
                    .set_delay(Duration::from_secs(10))
            })
            .expect(2)
            .mount(&server)
            .await;
        let device = Device::new(Some(&server.uri()), None, Duration::from_secs(15)).unwrap();
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let operation_guard = Arc::new(lock.clone().lock_owned().await);
        let task = tokio::spawn(async move {
            let mut cleanup = StreamCleanup {
                device,
                operation_guard: Some(operation_guard),
                armed: true,
            };
            cleanup.finish().await
        });
        tokio::time::timeout(Duration::from_secs(2), stopping.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), stopping.notified())
            .await
            .unwrap();
        assert!(
            lock.try_lock().is_err(),
            "in-flight stop cleanup must retain the lock"
        );
        let _guard = tokio::time::timeout(Duration::from_secs(5), lock.lock())
            .await
            .expect("stop cleanup must be bounded independently of HTTP timeout");
        server.verify().await;
    }
}
