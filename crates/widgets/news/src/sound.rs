//! The sound a pane makes when news arrives, and how often it may make it.
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
//! behind, which is a non-blocking send and never a wait.
//!
//! # Why a rate limit
//!
//! News arrives in runs. A service that publishes a batch is a reader who would otherwise
//! hear one clip per item, and the point of the sound is to say "something arrived", not
//! to count it. The window is measured from the last play that was actually heard, so a
//! burst is one sound and the next one is a full window later, rather than a burst
//! releasing a sound every time the window happens to roll over.

use std::{
    error::Error,
    io::Cursor,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use base_ui::assets::Assets;
use rodio::{Decoder, Source, buffer::SamplesBuffer, stream::DeviceSinkBuilder};
use tokio::sync::watch;

/// How often a pane may make itself heard.
///
/// The window is what keeps a run of reports from becoming a run of clips: one sound says
/// everything a reader needs from a burst, and the next one waits its turn.
pub(super) const MINIMUM_INTERVAL: Duration = Duration::from_secs(10);

/// The clip a pane plays, as it is embedded in the assets.
const NOTIFICATION_CLIP: &str = "sounds/news_notification.wav";

/// The name the thread that owns an output device is given.
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

    /// Whether this sound can be heard at all: an output device is open and ready.
    fn is_available(&self) -> bool;

    /// Watches that answer, waiting on it with `wait_for(|ready| *ready)`.
    ///
    /// The device is opened on the sound's own thread, which is a moment no frame knows
    /// about: a pane that draws its control from [`is_available`](Self::is_available) is a
    /// pane drawing the answer it read the first time until something wakes it. This is that
    /// something, and it is a wait that *ends* — both when the device opens and when the
    /// sound never has one — because a pane waiting on a device for as long as it lived
    /// would be a pane whose frames never settle.
    fn ready(&self) -> watch::Receiver<bool>;
}

/// A sound whose clip a pane hears again and again without one device per play.
pub(super) struct SoundPlayer {
    /// The requests left for the thread that owns the device. A send never blocks a frame.
    requests: SyncSender<()>,
    /// What the thread says about its device, which it has opened by the time this says so.
    /// It is state two threads share rather than a message either waits for: nothing in a
    /// pane blocks on a device, it only reads whether there is one.
    device: watch::Receiver<bool>,
}

impl SoundPlayer {
    /// Builds the player, and with it the thread that will own the device.
    ///
    /// Building is not playing: the thread takes the device and decodes the clip in its
    /// own time, and a pane that never hears news never waits for either.
    pub(super) fn new() -> Self {
        // One request outstanding is one clip: a request already waiting is the sound this
        // one would have made, so a deeper queue would be the same news twice.
        let (requests, receiver) = mpsc::sync_channel(1);
        let (status, device) = watch::channel(false);

        // The thread owns itself: nothing joins it, because what it is doing when a pane
        // goes away is opening a device — a wait that has no business on a frame's thread.
        // It ends on its own when its request channel closes, which dropping this player is.
        if let Err(error) = thread::Builder::new()
            .name(DEVICE_THREAD.to_owned())
            .spawn(move || serve(receiver, &status))
        {
            log::warn!("the notification sound has no thread: {error}");
        }

        Self { requests, device }
    }
}

impl NotificationSound for SoundPlayer {
    fn play(&self) {
        // A request the thread is not there for is a sound with no device to be heard on,
        // which is what [`is_available`](Self::is_available) is asked about: a play that
        // arrived anyway is dropped rather than reported, because the pane that asked for
        // it reports the same fact on the frame that draws its control.
        let _ = self.requests.try_send(());
    }

    fn is_available(&self) -> bool {
        *self.device.borrow()
    }

    /// A wait that ends either way: on the device that arrives, and on the thread that
    /// leaves without one, because a sender that goes with it is the same answer.
    fn ready(&self) -> watch::Receiver<bool> {
        self.device.clone()
    }
}

/// Owns an output device for as long as it is asked to play, which is the whole point.
fn serve(requests: Receiver<()>, device: &watch::Sender<bool>) {
    let clip = match open_and_decode() {
        Ok(clip) => clip,
        Err(error) => {
            log::warn!("the notification sound is unavailable: {error}");
            return;
        }
    };

    // The device is open and the clip is in memory, so every request from here on is heard:
    // this is what a pane reads to know it has a sound to offer, and it is sent before the
    // first request can be answered rather than with it.
    device.send_replace(true);
    while requests.recv().is_ok() {
        if let Err(error) = clip.play() {
            log::warn!("the notification sound stopped playing: {error}");
            break;
        }
    }
    // The device goes with this thread, so what a pane reads from here on is a sound it may
    // not offer — the same fact its request channel closing says, said where a frame reads it.
    device.send_replace(false);
}

/// An output device, and the clip it is to play.
struct Device {
    /// Dropping the sink is what closes the device, so a pane holds it for its whole life.
    sink: rodio::MixerDeviceSink,
    channels: rodio::ChannelCount,
    sample_rate: rodio::SampleRate,
    samples: Arc<[f32]>,
}

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
            self.samples.to_vec(),
        ));

        Ok(())
    }
}

/// Opens the default output device and reads the clip into it.
fn open_and_decode() -> Result<Device, Box<dyn Error>> {
    let mut sink = DeviceSinkBuilder::open_default_sink()?;
    // A pane closes its device whenever the pane goes, which is not something a reader
    // needs telling: what rodio would print there is the sound the pane already stopped
    // making, and the pane's own header is where that is said.
    sink.log_on_drop(false);
    let (channels, sample_rate, samples) = notification_samples()?;

    Ok(Device {
        sink,
        channels,
        sample_rate,
        samples: samples.into(),
    })
}

/// The clip a pane plays, decoded once.
fn notification_samples()
-> Result<(rodio::ChannelCount, rodio::SampleRate, Vec<f32>), Box<dyn Error>> {
    let clip = Assets::get(NOTIFICATION_CLIP)
        .ok_or_else(|| format!("the clip `{NOTIFICATION_CLIP}` is not in the embedded assets"))?;
    let decoder = Decoder::try_from(Cursor::new(clip.data))?;
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

    /// The clip is embedded rather than loaded from disk, so this is the test that says
    /// the path a pane plays is one the binary actually holds — a typo here is a sound
    /// nobody ever hears, which is not something a build would otherwise notice.
    #[test]
    fn the_embedded_clip_should_decode_to_the_samples_a_device_plays() {
        let (channels, sample_rate, samples) =
            notification_samples().expect("the notification clip should be in the embedded assets");

        assert_eq!(channels.get(), 2, "the clip is a stereo recording");
        assert_eq!(
            sample_rate.get(),
            44_100,
            "the clip is a 44.1 kHz recording"
        );

        // The clip is 1.96 seconds of stereo audio, which is what a reader hears: a guard
        // that only asked for "longer than a click" would pass for a truncated file or for
        // the wrong clip entirely, and neither is something a build would notice.
        let frames = samples.len() / usize::from(channels.get());
        let seconds = frames as f64 / f64::from(sample_rate.get());
        assert!(
            (1.8..2.2).contains(&seconds),
            "the notification is about two seconds long: {seconds} s"
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
}
