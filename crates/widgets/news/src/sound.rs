//! The sound a pane makes when news arrives, and how often it may make it.
//!
//! # Who owns the clip
//!
//! The application owns its audio assets (design §9.5), so a pane does not read one out
//! of the component library: the host hands the bytes over through
//! [`WidgetServices::sound`](widget::services::WidgetServices::sound), and `None` means
//! this host has no audio at all. That is the ordinary case in a test and never an
//! error: a pane with no sound draws its control disabled and announces nothing.
//!
//! # Why a device outlives a play
//!
//! Playing a sound is not the expensive part of hearing one: opening an output device is.
//! A device is opened on the thread that owns it, once, and that thread is what a pane
//! holds for as long as the pane lives — a play is nothing but a hand-off to it, so the
//! news a reader is told about is heard within the buffer the device is already running
//! on rather than after a device has been opened, configured and started again. The clip
//! is decoded for the same reason: it is read once, and every play after that is a copy
//! of samples that are already in memory.
//!
//! Both are also why nothing here belongs on the frame's thread. The device is opened,
//! and the clip decoded, on the thread built for them; a pane only ever leaves a request
//! behind, which is a non-blocking send and never a wait. A host whose bytes turn out not
//! to be audio, or whose device never opens, loses the sound — not the frame.
//!
//! # Why a rate limit
//!
//! News arrives in runs. A service that publishes a batch is a reader who would otherwise
//! hear one clip per item, and the point of the sound is to say "something arrived", not
//! to count it. The window is measured from the last play that was actually heard, so a
//! burst is one sound and the next one is a full window later, rather than a burst
//! releasing a sound every time the window happens to roll over.
//!
//! # The backend is a feature
//!
//! The device half is behind this crate's `audio` feature (`rodio` -> `cpal` -> ALSA on
//! Linux): the acceptance build (`cargo build -p kytrix --no-default-features --features
//! acceptance`) runs on hosts where ALSA's development files are not installed, so it
//! links no audio backend at all. Everything above the device — the seam, the throttle and
//! the silent host — is backend-free and always compiled, so a pane in a build without
//! audio draws exactly what a pane with audio draws, it just has nothing to play.

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

/// How often a pane may make itself heard.
///
/// The window is what keeps a run of reports from becoming a run of clips: one sound says
/// everything a reader needs from a burst, and the next one waits its turn.
pub(super) const MINIMUM_INTERVAL: Duration = Duration::from_secs(10);

/// The name of the clip a pane asks the host for.
///
/// It is a key into the application's own asset table, not a path this crate reads. A build
/// without the `audio` feature never asks for one: it has no backend to hand it to.
#[cfg(feature = "audio")]
pub(super) const NOTIFICATION_CLIP: &str = "sounds/news_notification.wav";

/// The name the thread that owns an output device is given.
#[cfg(feature = "audio")]
const DEVICE_THREAD: &str = "news-notification-sound";

/// What a pane asks of the sound it was built with.
///
/// It is a seam rather than a type because the answer is about the machine the pane runs
/// on: a test drives a pane on a machine it may not open a device on, and what it needs
/// to know is what the pane *asked* for, which is exactly what an implementation of this
/// is handed.
pub(super) trait NotificationSound: Send + Sync + 'static {
    /// Asks for the clip to be played, and returns immediately.
    fn play(&self);

    /// Whether this pane has a clip to be heard at all.
    ///
    /// It is the host's answer, not the device's: the bytes were supplied means the
    /// application has audio, and a device that then fails is a play that is dropped
    /// rather than a control that changes under the reader.
    fn is_available(&self) -> bool;
}

/// A sound whose clip a pane hears again and again without one device per play.
#[cfg(feature = "audio")]
pub(super) struct SoundPlayer {
    /// The requests left for the thread that owns the device. A send never blocks a frame.
    requests: SyncSender<()>,
}

#[cfg(feature = "audio")]
impl SoundPlayer {
    /// Builds the player, and with it the thread that will own the device.
    ///
    /// Building is not playing: the thread takes the device and decodes the clip in its
    /// own time, and a pane that never hears news never waits for either.
    pub(super) fn new(clip: Bytes) -> Self {
        // One request outstanding is one clip: a request already waiting is the sound this
        // one would have made, so a deeper queue would be the same news twice.
        let (requests, receiver) = mpsc::sync_channel(1);

        // The thread owns itself: nothing joins it, because what it is doing when a pane
        // goes away is opening a device — a wait that has no business on a frame's thread.
        // It ends on its own when its request channel closes, which dropping this player is.
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
        // A request the thread is not there for is a sound with no device to be heard on,
        // which is what [`is_available`](Self::is_available) is asked about: a play that
        // arrived anyway is dropped rather than reported, because the pane that asked for
        // it reports the same fact on the frame that draws its control.
        let _ = self.requests.try_send(());
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// The sound of a host that answered `None` for the clip: there is no audio here.
///
/// It is a real implementation rather than an `Option` so the pane's announcement path
/// does not branch on the host it is running on: the one thing it does differently is
/// nothing.
pub(super) struct SilentSound;

impl NotificationSound for SilentSound {
    fn play(&self) {}

    fn is_available(&self) -> bool {
        false
    }
}

/// Owns an output device for as long as it is asked to play, which is the whole point.
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

/// An output device, and the clip it is to play.
#[cfg(feature = "audio")]
struct Device {
    /// Dropping the sink is what closes the device, so a pane holds it for its whole life.
    sink: rodio::MixerDeviceSink,
    channels: rodio::ChannelCount,
    sample_rate: rodio::SampleRate,
    samples: Vec<f32>,
}

#[cfg(feature = "audio")]
impl Device {
    /// Plays the clip over the device that is already open.
    ///
    /// A buffer is a copy of samples the device's mixer converts to its own rate and
    /// channel count, so nothing here waits on the device: the samples join the buffer it
    /// is already playing from.
    fn play(&self) -> Result<(), rodio::PlayError> {
        self.sink.mixer().add(SamplesBuffer::new(
            self.channels,
            self.sample_rate,
            self.samples.clone(),
        ));

        Ok(())
    }
}

/// Opens the default output device and reads the clip into it.
#[cfg(feature = "audio")]
fn open_and_decode(clip: &[u8]) -> Result<Device, Box<dyn Error>> {
    let mut sink = DeviceSinkBuilder::open_default_sink()?;
    // A pane closes its device whenever the pane goes, which is not something a reader
    // needs telling: what rodio would print there is the sound the pane already stopped
    // making, and the pane's own header is where that is said.
    sink.log_on_drop(false);
    let (channels, sample_rate, samples) = notification_samples(clip)?;

    Ok(Device {
        sink,
        channels,
        sample_rate,
        samples,
    })
}

/// The clip a pane plays, decoded once.
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

/// Whether a pane may be heard now, and when it was last heard.
#[derive(Default)]
pub(super) struct Throttle {
    /// When a clip was last handed to the device, or `None` for one that never was.
    last: Option<Instant>,
}

impl Throttle {
    /// The state a throttle is in `ago` after it last let one through.
    ///
    /// A clock is not something a test can wind on the pane's behalf: what it can say is
    /// how long ago the sound it is testing against was heard.
    #[cfg(test)]
    pub(super) fn heard_at(ago: Duration) -> Self {
        Self {
            last: Some(Instant::now() - ago),
        }
    }

    /// Whether a play that is happening now is one a reader should hear.
    ///
    /// The window is measured from the play that was heard rather than from the news that
    /// asked for it: a burst is one sound, and the sound after it is a window after it.
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

    /// The window runs from the play that was heard, not from the request that was
    /// refused: otherwise a burst of news would release a clip every time the window
    /// happened to roll over.
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

    /// A 16-bit PCM WAV of `frames` stereo frames at 44.1 kHz, as the application's own
    /// clip would arrive: the same reader the pane uses is what decodes it.
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

    /// The clip is bytes the host handed over, so this is the test that says the decoder
    /// a play runs through is the reader the application's WAV format needs: a clip that
    /// a `curl` and a text editor would accept must not decode to silence.
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

        // And what a play hands the device is those samples: a buffer is the clip as a
        // source, and a source that dropped or reordered them would be a sound nobody
        // recognises rather than a failure.
        let buffer = SamplesBuffer::new(channels, sample_rate, samples.clone());
        assert_eq!(buffer.count(), samples.len());
    }

    /// A host whose asset is not audio, or whose device never opens, loses the sound —
    /// not the frame: building the player and asking it to play must not panic.
    #[cfg(feature = "audio")]
    #[test]
    fn a_clip_that_is_not_audio_should_be_dropped_instead_of_panicking() {
        assert!(notification_samples(b"not a wav").is_err());

        let player = SoundPlayer::new(Bytes::from_static(b"not a wav"));
        player.play();
        assert!(player.is_available(), "the host did supply bytes");
    }

    /// A host that answered `None` has no sound, and asking it for one is a no-op: the
    /// pane's announcement path is the same on every host.
    #[test]
    fn a_host_without_audio_should_answer_a_sound_that_is_silent() {
        let sound = SilentSound;

        sound.play();
        assert!(!sound.is_available(), "no bytes means no sound to offer");
    }
}
