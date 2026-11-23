use std::time::{Duration, Instant};
#[cfg(feature = "audio")]
use std::{
    error::Error,
    io::Cursor,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};

#[cfg(feature = "audio")]
use bytes::Bytes;
#[cfg(feature = "audio")]
use rodio::{Decoder, Source, buffer::SamplesBuffer, stream::DeviceSinkBuilder};

pub(super) const MINIMUM_INTERVAL: Duration = Duration::from_secs(10);

#[cfg(feature = "audio")]
pub(super) const NOTIFICATION_CLIP: &str = "sounds/news_notification.wav";

#[cfg(feature = "audio")]
const DEVICE_THREAD: &str = "news-notification-sound";

pub(super) trait NotificationSound: Send + Sync + 'static {
    fn play(&self);

    fn is_available(&self) -> bool;
}

#[cfg(feature = "audio")]
pub(super) struct SoundPlayer {
    requests: SyncSender<()>,
}

#[cfg(feature = "audio")]
impl SoundPlayer {
    pub(super) fn new(clip: Bytes) -> Self {
        let (requests, receiver) = mpsc::sync_channel(1);

        if let Err(error) = thread::Builder::new()
            .name(DEVICE_THREAD.to_owned())
            .spawn(move || serve(receiver, clip))
        {
            log::warn!("the notification sound has no thread: {error}");
        }

        Self { requests }
    }
}

#[cfg(feature = "audio")]
impl NotificationSound for SoundPlayer {
    fn play(&self) {
        let _ = self.requests.try_send(());
    }

    fn is_available(&self) -> bool {
        true
    }
}

pub(super) struct SilentSound;

impl NotificationSound for SilentSound {
    fn play(&self) {}

    fn is_available(&self) -> bool {
        false
    }
}

#[cfg(feature = "audio")]
fn serve(requests: Receiver<()>, clip: Bytes) {
    let device = match open_and_decode(&clip) {
        Ok(device) => device,
        Err(error) => {
            log::warn!("the notification sound is unavailable: {error}");
            return;
        }
    };

    while requests.recv().is_ok() {
        if let Err(error) = device.play() {
            log::warn!("the notification sound stopped playing: {error}");
            break;
        }
    }
}

#[cfg(feature = "audio")]
struct Device {
    sink: rodio::MixerDeviceSink,
    channels: rodio::ChannelCount,
    sample_rate: rodio::SampleRate,
    samples: Vec<f32>,
}

#[cfg(feature = "audio")]
impl Device {
    fn play(&self) -> Result<(), rodio::PlayError> {
        self.sink.mixer().add(SamplesBuffer::new(
            self.channels,
            self.sample_rate,
            self.samples.clone(),
        ));

        Ok(())
    }
}

#[cfg(feature = "audio")]
fn open_and_decode(clip: &[u8]) -> Result<Device, Box<dyn Error>> {
    let mut sink = DeviceSinkBuilder::open_default_sink()?;
    sink.log_on_drop(false);
    let (channels, sample_rate, samples) = notification_samples(clip)?;

    Ok(Device {
        sink,
        channels,
        sample_rate,
        samples,
    })
}

#[cfg(feature = "audio")]
fn notification_samples(
    clip: &[u8],
) -> Result<(rodio::ChannelCount, rodio::SampleRate, Vec<f32>), Box<dyn Error>> {
    let decoder = Decoder::try_from(Cursor::new(clip.to_vec()))?;
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    let samples = decoder.collect();

    Ok((channels, sample_rate, samples))
}

#[derive(Default)]
pub(super) struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    #[cfg(test)]
    pub(super) fn heard_at(ago: Duration) -> Self {
        Self {
            last: Some(Instant::now() - ago),
        }
    }

    pub(super) fn allows(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now.saturating_duration_since(last) < MINIMUM_INTERVAL => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_play_should_be_allowed() {
        let mut throttle = Throttle::default();

        assert!(throttle.allows(Instant::now()));
    }

    #[test]
    fn a_play_inside_the_window_should_be_refused() {
        let started = Instant::now();
        let mut throttle = Throttle::default();
        assert!(throttle.allows(started));

        assert!(
            !throttle.allows(started + MINIMUM_INTERVAL - Duration::from_millis(1)),
            "a clip is not heard again before the window is up"
        );
    }

    #[test]
    fn a_play_a_window_later_should_be_allowed() {
        let started = Instant::now();
        let mut throttle = Throttle::default();
        assert!(throttle.allows(started));

        assert!(throttle.allows(started + MINIMUM_INTERVAL));
    }

    #[test]
    fn a_refused_play_should_not_move_the_window() {
        let started = Instant::now();
        let mut throttle = Throttle::default();
        assert!(throttle.allows(started));
        assert!(!throttle.allows(started + MINIMUM_INTERVAL - Duration::from_millis(1)));

        assert!(
            throttle.allows(started + MINIMUM_INTERVAL),
            "the window belongs to the play that was heard"
        );
    }

    #[cfg(feature = "audio")]
    fn stereo_44k_wav(frames: u32) -> Vec<u8> {
        const CHANNELS: u16 = 2;
        const SAMPLE_RATE: u32 = 44_100;
        const BITS: u16 = 16;
        let samples = frames * u32::from(CHANNELS);
        let data_len = samples * u32::from(BITS / 8);
        let byte_rate = SAMPLE_RATE * u32::from(CHANNELS) * u32::from(BITS / 8);
        let block_align = CHANNELS * (BITS / 8);

        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&CHANNELS.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&BITS.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..samples {
            let sample = i16::try_from(frame % 4).expect("the fixture's amplitude fits i16");
            wav.extend_from_slice(&sample.to_le_bytes());
        }

        wav
    }

    #[cfg(feature = "audio")]
    #[test]
    fn the_clip_the_host_hands_over_should_decode_to_the_samples_a_device_plays() {
        let wav = stereo_44k_wav(441);
        let (channels, sample_rate, samples) =
            notification_samples(&wav).expect("a PCM WAV should decode");

        assert_eq!(channels.get(), 2, "the clip is a stereo recording");
        assert_eq!(
            sample_rate.get(),
            44_100,
            "the clip is a 44,1 kHz recording"
        );
        assert_eq!(
            samples.len(),
            441 * 2,
            "every frame the clip holds reaches the device"
        );
        assert!(
            samples.iter().all(|sample| sample.is_finite()),
            "a clip with a sample that is not a number is not one a device can play"
        );

        let buffer = SamplesBuffer::new(channels, sample_rate, samples.clone());
        assert_eq!(buffer.count(), samples.len());
    }

    #[cfg(feature = "audio")]
    #[test]
    fn a_clip_that_is_not_audio_should_be_dropped_instead_of_panicking() {
        assert!(notification_samples(b"not a wav").is_err());

        let player = SoundPlayer::new(Bytes::from_static(b"not a wav"));
        player.play();
        assert!(player.is_available(), "the host did supply bytes");
    }

    #[test]
    fn a_host_without_audio_should_answer_a_sound_that_is_silent() {
        let sound = SilentSound;

        sound.play();
        assert!(!sound.is_available(), "no bytes means no sound to offer");
    }
}
