//! Shared libav send_packet/EAGAIN handling for video and audio decoders.
use ffmpeg_next as av;

/// EAGAIN leaves this input unconsumed: drain output, then submit it again.
pub(super) fn send_decoder_packet(
    decoder: &mut av::codec::decoder::Opened,
    packet: Option<&av::Packet>,
) -> std::result::Result<bool, av::Error> {
    let sent = match packet {
        Some(packet) => decoder.send_packet(packet),
        None => decoder.send_eof(),
    };
    match sent {
        Ok(()) => Ok(true),
        Err(av::Error::Eof) if packet.is_none() => Ok(true),
        Err(av::Error::Other { errno }) if errno == libc::EAGAIN => Ok(false),
        Err(error) => Err(error),
    }
}
