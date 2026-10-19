//! Voice input for the Assistant: one open microphone and one loaded speech model,
//! turned into one transcript per push-to-talk turn (docs/assistant/voice-input.md).
//!
//! Two threads. The capture thread owns the microphone and is the only producer of turn
//! boundaries: `begin_turn`, `end_turn` and `cancel_turn` travel in the same queue as the
//! microphone's blocks, so each boundary falls exactly between the blocks captured before
//! and after the call (plan §7.2, Codex F6). It resamples to 16 kHz and writes a turn's
//! audio, then the boundary, into the ASR queue. The ASR worker handles that queue
//! strictly in order and resets the model after every turn.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use parakeet_rs::MultitalkerASR;

use crate::asr_local::{load_model, SAMPLE_RATE};
use crate::audio_capture::InputStream;
use crate::audio_decode::StreamResampler;

/// Audio kept from just before `begin_turn`, so a word started a moment before the press
/// is not clipped. It also means up to this much of whatever played before the press
/// (for example the end of a spoken answer through loudspeakers) can reach the turn.
const PRE_ROLL: Duration = Duration::from_millis(300);
/// A turn is ended automatically at this length; see [`VoiceSession`].
const MAX_TURN: Duration = Duration::from_secs(120);
/// Messages the ASR queue holds. Audio goes in blocks of at least [`FORWARD_SAMPLES`]
/// (only a turn's last block is shorter), so this is about 240 s of audio, twice the
/// longest turn. A full queue means the model cannot keep up and ends the session.
const ASR_QUEUE_MESSAGES: usize = 2_400;
/// The capture thread forwards audio in blocks of this many 16 kHz samples (100 ms).
const FORWARD_SAMPLES: usize = SAMPLE_RATE as usize / 10;
/// Samples per `level` report: 20 reports a second.
const LEVEL_SAMPLES: usize = SAMPLE_RATE as usize / 20;
/// Silence chunks after a turn's last chunk, so the model emits the words it holds back.
/// The same flush as the end of a Transcribe recording (`asr_local::run`).
const FLUSH_SILENCE_CHUNKS: usize = 3;
/// A microphone that delivers nothing for this long is treated as broken (the same limit
/// as Transcribe's; a Bluetooth headset can take over a second to start).
const SILENT_DEVICE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often each thread looks at the stop flag while it waits.
const POLL: Duration = Duration::from_millis(50);
/// How long `close` waits for the threads before leaving them to finish on their own.
const JOIN_TIMEOUT: Duration = Duration::from_secs(3);

/// Where a session's results go. Called from the session's threads.
pub(crate) trait VoiceEvents: Send + Sync + 'static {
    /// RMS (0 to 1) of the last 50 ms, about 20 times a second while a turn is open.
    fn level(&self, rms: f32);
    /// The text so far for the open turn, after each chunk that changed it.
    fn partial(&self, turn_id: u64, text: &str);
    /// The whole text of an ended turn (possibly empty), exactly once per ended turn.
    fn final_text(&self, turn_id: u64, text: &str);
    /// The session stopped working. Called at most once; every later call fails.
    fn error(&self, message: &str);
    /// Automatic turn-taking began (`started`) or ended a turn.
    fn auto_turn(&self, _turn_id: u64, _started: bool) {}
    /// Automatic turn-taking failed and was switched off; push-to-talk still works.
    fn auto_failed(&self, _message: &str) {}
}

/// What an automatic turn detector reports (see `assistant_turn::TurnDetector`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AutoTurnEvent {
    SpeechStarted,
    EndOfTurn,
}

/// Decides turn boundaries from the audio itself. It runs **inside the capture thread** and
/// sees each block before that block is routed, so a boundary falls exactly between blocks,
/// as push-to-talk's does (Codex review of the voice pipeline, F1).
pub(crate) trait TurnDecider: Send {
    fn push(&mut self, samples: &[f32]) -> Result<Vec<AutoTurnEvent>>;
    /// Forgets what it has heard (called when it resumes after a pause).
    fn reset(&mut self);
}

/// The speech model as the worker sees it, so the turn logic can be tested without it.
pub(crate) trait AsrEngine: Send {
    /// 16 kHz mono samples per call to [`feed`](Self::feed).
    fn chunk_samples(&self) -> usize;
    /// Transcribes one chunk and returns the utterance's whole text so far.
    fn feed(&mut self, chunk: &[f32]) -> Result<String>;
    /// Transcribes `tail` (shorter than a chunk) padded with silence, then silence, so the
    /// model emits the words it was still holding back. Returns the whole text.
    fn flush(&mut self, tail: &[f32]) -> Result<String> {
        let chunk = self.chunk_samples();
        let mut last = tail.to_vec();
        last.resize(chunk, 0.0);
        let mut text = self.feed(&last)?;
        let silence = vec![0.0f32; chunk];
        for _ in 0..FLUSH_SILENCE_CHUNKS {
            text = self.feed(&silence)?;
        }
        Ok(text)
    }
    /// Forgets the utterance, so the next one starts from a clean state.
    fn reset(&mut self);
}

/// The multitalker Parakeet model, loaded for one voice like `transcribe_clip`.
struct ParakeetEngine(MultitalkerASR);

impl AsrEngine for ParakeetEngine {
    fn chunk_samples(&self) -> usize {
        self.0.chunk_audio_samples()
    }

    fn feed(&mut self, chunk: &[f32]) -> Result<String> {
        self.0.transcribe_chunk(chunk).map_err(|err| anyhow!("Speech recognition failed: {err}"))?;
        // Each speaker the diarizer heard keeps its own text; joining them in order of
        // first appearance never splices one speaker's half word into another's.
        Ok(join_words(self.0.get_transcripts().iter().map(|speaker| speaker.text.as_str())))
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

fn join_words<'a>(texts: impl Iterator<Item = &'a str>) -> String {
    texts.flat_map(str::split_whitespace).collect::<Vec<_>>().join(" ")
}

/// What the capture thread receives: the microphone's blocks at the device rate, and the
/// session's turn boundaries, in one queue.
enum CaptureMsg {
    Block(Vec<f32>),
    Begin(u64),
    End(u64),
    Cancel(u64),
    /// Sets (or clears) the automatic turn detector, and the pre-roll length in samples.
    SetAuto(Option<Box<dyn TurnDecider>>, usize),
    /// Pauses (true) or resumes (false) the installed detector without unloading it.
    PauseAuto(bool),
    Close,
}

impl From<Vec<f32>> for CaptureMsg {
    fn from(block: Vec<f32>) -> Self {
        Self::Block(block)
    }
}

/// What the ASR worker receives, in recording order.
#[derive(Debug, PartialEq)]
enum AsrMsg {
    Audio { turn_id: u64, samples: Vec<f32> },
    EndTurn(u64),
    CancelTurn(u64),
}

/// Lengths in 16 kHz samples.
#[derive(Clone, Copy)]
struct Limits {
    pre_roll: usize,
    max_turn: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pre_roll: (PRE_ROLL.as_secs_f32() * SAMPLE_RATE as f32) as usize,
            max_turn: MAX_TURN.as_secs() as usize * SAMPLE_RATE as usize,
        }
    }
}

/// State both threads and the session handle share.
struct Shared {
    events: Arc<dyn VoiceEvents>,
    failure: Mutex<Option<String>>,
    stopping: AtomicBool,
    /// Turn ids for push-to-talk and automatic turns alike, so they always increase.
    next_turn: Arc<std::sync::atomic::AtomicU64>,
}

impl Shared {
    /// Records the first failure and reports it; later ones, and any during close, are dropped.
    fn fail(&self, message: &str) {
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut failure) = self.failure.lock() else { return };
        if failure.is_some() {
            return;
        }
        *failure = Some(message.to_string());
        drop(failure);
        self.events.error(message);
    }

    fn failure(&self) -> Option<String> {
        self.failure.lock().map(|failure| failure.clone()).unwrap_or_else(|_| Some("Voice input state lock poisoned".to_string()))
    }

    fn should_stop(&self) -> bool {
        self.stopping.load(Ordering::SeqCst) || self.failure().is_some()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TurnState {
    Idle,
    Open(u64),
    /// Ended automatically at the length limit; audio is discarded until the caller ends it.
    Truncated(u64),
}

/// The capture thread's logic, separate from the thread so it can be tested directly.
struct CaptureStage {
    source_rate: u32,
    resampler: StreamResampler,
    limits: Limits,
    turn: TurnState,
    /// The last `limits.pre_roll` samples heard while no turn was open.
    pre_roll: VecDeque<f32>,
    /// Audio of the open turn not yet forwarded.
    outgoing: Vec<f32>,
    turn_samples: usize,
    level_sum: f64,
    level_count: usize,
    asr: SyncSender<AsrMsg>,
    events: Arc<dyn VoiceEvents>,
    /// See [`CaptureMsg::SetAuto`].
    auto: Option<Box<dyn TurnDecider>>,
    /// The turn the detector began, while it is open.
    auto_open: Option<u64>,
    /// While paused the detector hears nothing (the microphone is "closed" during an answer).
    auto_paused: bool,
    /// The pre-roll to use while the detector runs, and the one to restore when paused.
    auto_pre_roll: usize,
    manual_pre_roll: usize,
    next_turn: Arc<std::sync::atomic::AtomicU64>,
}

impl CaptureStage {
    fn new(
        source_rate: u32,
        limits: Limits,
        asr: SyncSender<AsrMsg>,
        events: Arc<dyn VoiceEvents>,
        next_turn: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            source_rate,
            resampler: StreamResampler::new(source_rate, SAMPLE_RATE),
            limits,
            turn: TurnState::Idle,
            pre_roll: VecDeque::new(),
            outgoing: Vec::new(),
            turn_samples: 0,
            level_sum: 0.0,
            level_count: 0,
            asr,
            events,
            auto: None,
            auto_open: None,
            auto_paused: false,
            auto_pre_roll: limits.pre_roll,
            manual_pre_roll: limits.pre_roll,
            next_turn,
        }
    }

    fn block(&mut self, block: &[f32]) -> Result<()> {
        let mut resampled = Vec::with_capacity(block.len());
        self.resampler.push(block, &mut resampled);
        // The detector judges this block before it is routed: a turn it begins includes the
        // block (and the pre-roll), and a turn it ends includes the block and nothing later.
        let mut end_after: Option<u64> = None;
        if let Some(decider) = self.auto.as_mut().filter(|_| !self.auto_paused) {
            match decider.push(&resampled) {
                Ok(events) => {
                    for event in events {
                        match event {
                            AutoTurnEvent::SpeechStarted if self.turn == TurnState::Idle => {
                                let turn_id = self.next_turn.fetch_add(1, Ordering::SeqCst) + 1;
                                self.control(CaptureMsg::Begin(turn_id))?;
                                self.auto_open = Some(turn_id);
                                self.events.auto_turn(turn_id, true);
                            }
                            AutoTurnEvent::EndOfTurn => end_after = end_after.or(self.auto_open),
                            AutoTurnEvent::SpeechStarted => {}
                        }
                    }
                }
                Err(err) => {
                    self.auto = None;
                    self.events.auto_failed(&format!("Automatic sending stopped: {err:#}"));
                }
            }
        }
        self.route(resampled)?;
        if let Some(turn_id) = end_after {
            match self.turn {
                TurnState::Open(open) if open == turn_id => {
                    self.end_open(turn_id)?;
                    self.turn = TurnState::Idle;
                }
                TurnState::Truncated(open) if open == turn_id => self.turn = TurnState::Idle,
                _ => {}
            }
            self.auto_open = None;
            self.events.auto_turn(turn_id, false);
        }
        Ok(())
    }

    fn route(&mut self, resampled: Vec<f32>) -> Result<()> {
        match self.turn {
            TurnState::Idle => {
                self.pre_roll.extend(resampled);
                let excess = self.pre_roll.len().saturating_sub(self.limits.pre_roll);
                self.pre_roll.drain(..excess);
            }
            TurnState::Open(turn_id) => {
                self.report_level(&resampled);
                self.turn_samples += resampled.len();
                self.outgoing.extend(resampled);
                if self.turn_samples >= self.limits.max_turn {
                    self.end_open(turn_id)?;
                    self.turn = TurnState::Truncated(turn_id);
                } else if self.outgoing.len() >= FORWARD_SAMPLES {
                    self.forward(turn_id)?;
                }
            }
            TurnState::Truncated(_) => {}
        }
        Ok(())
    }

    fn control(&mut self, message: CaptureMsg) -> Result<()> {
        match (message, self.turn) {
            (CaptureMsg::Begin(turn_id), turn) => {
                // The session never sends this while a turn is open; end it rather than blend.
                if let TurnState::Open(open) = turn {
                    self.end_open(open)?;
                    if self.auto_open == Some(open) {
                        self.auto_open = None;
                        self.events.auto_turn(open, false);
                    }
                }
                self.turn = TurnState::Open(turn_id);
                self.outgoing = self.pre_roll.drain(..).collect();
                self.turn_samples = self.outgoing.len();
                self.level_sum = 0.0;
                self.level_count = 0;
            }
            (CaptureMsg::End(turn_id), TurnState::Open(open)) if open == turn_id => {
                self.end_open(turn_id)?;
                self.turn = TurnState::Idle;
            }
            (CaptureMsg::Cancel(turn_id), TurnState::Open(open)) if open == turn_id => {
                self.outgoing.clear();
                self.resampler = StreamResampler::new(self.source_rate, SAMPLE_RATE);
                self.send(AsrMsg::CancelTurn(turn_id))?;
                self.turn = TurnState::Idle;
            }
            (CaptureMsg::End(turn_id) | CaptureMsg::Cancel(turn_id), TurnState::Truncated(open)) if open == turn_id => {
                self.turn = TurnState::Idle;
            }
            (CaptureMsg::SetAuto(decider, pre_roll), _) => {
                if decider.is_none() {
                    self.cancel_auto_turn()?;
                    self.limits.pre_roll = self.manual_pre_roll;
                } else {
                    self.auto_pre_roll = pre_roll;
                    self.limits.pre_roll = pre_roll;
                }
                self.auto = decider;
                self.auto_paused = false;
            }
            (CaptureMsg::PauseAuto(paused), _) => {
                if paused {
                    // The page pauses it when an answer starts, after the turn it sent ended.
                    self.cancel_auto_turn()?;
                    self.limits.pre_roll = self.manual_pre_roll;
                } else {
                    if let Some(decider) = self.auto.as_mut() {
                        decider.reset();
                    }
                    self.pre_roll.clear();
                    self.limits.pre_roll = self.auto_pre_roll;
                }
                self.auto_paused = paused;
            }
            _ => {}
        }
        Ok(())
    }

    /// Discards a turn the detector had begun and is still open.
    fn cancel_auto_turn(&mut self) -> Result<()> {
        if let (Some(auto), TurnState::Open(open)) = (self.auto_open, self.turn) {
            if auto == open {
                self.outgoing.clear();
                self.resampler = StreamResampler::new(self.source_rate, SAMPLE_RATE);
                self.send(AsrMsg::CancelTurn(open))?;
                self.turn = TurnState::Idle;
                self.events.auto_turn(open, false);
            }
        }
        self.auto_open = None;
        Ok(())
    }

    /// Forwards the rest of the open turn, including the resampler's tail, then the boundary.
    fn end_open(&mut self, turn_id: u64) -> Result<()> {
        self.resampler.finish(&mut self.outgoing);
        self.resampler = StreamResampler::new(self.source_rate, SAMPLE_RATE);
        self.forward(turn_id)?;
        self.pre_roll.clear();
        self.send(AsrMsg::EndTurn(turn_id))
    }

    fn forward(&mut self, turn_id: u64) -> Result<()> {
        if self.outgoing.is_empty() {
            return Ok(());
        }
        let samples = std::mem::take(&mut self.outgoing);
        self.send(AsrMsg::Audio { turn_id, samples })
    }

    fn send(&self, message: AsrMsg) -> Result<()> {
        match self.asr.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(anyhow!("Speech recognition cannot keep up with the microphone.")),
            Err(TrySendError::Disconnected(_)) => Err(anyhow!("Speech recognition stopped.")),
        }
    }

    fn report_level(&mut self, samples: &[f32]) {
        for &sample in samples {
            self.level_sum += f64::from(sample) * f64::from(sample);
            self.level_count += 1;
            if self.level_count == LEVEL_SAMPLES {
                let rms = (self.level_sum / self.level_count as f64).sqrt() as f32;
                self.events.level(rms.clamp(0.0, 1.0));
                self.level_sum = 0.0;
                self.level_count = 0;
            }
        }
    }
}

/// The audio source as the capture thread sees it. It is created on that thread and
/// never leaves it, because a microphone stream cannot move between threads.
struct Input {
    source_rate: u32,
    /// Returns an error the audio system reported, if any.
    take_error: Box<dyn FnMut() -> Option<String>>,
    silent_timeout: Option<Duration>,
}

fn run_capture(mut input: Input, receiver: Receiver<CaptureMsg>, mut stage: CaptureStage, shared: &Shared) {
    let mut last_block = Instant::now();
    loop {
        if shared.should_stop() {
            return;
        }
        if let Some(message) = (input.take_error)() {
            shared.fail(&format!("The microphone stopped working: {message}"));
            return;
        }
        let result = match receiver.recv_timeout(POLL) {
            Ok(CaptureMsg::Block(block)) => {
                last_block = Instant::now();
                stage.block(&block)
            }
            Ok(CaptureMsg::Close) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(control) => stage.control(control),
            Err(RecvTimeoutError::Timeout) => match input.silent_timeout {
                Some(limit) if last_block.elapsed() > limit => Err(anyhow!(
                    "No audio is arriving from the microphone. Check that it is connected and that apps are allowed to use it."
                )),
                _ => Ok(()),
            },
        };
        if let Err(err) = result {
            shared.fail(&err.to_string());
            return;
        }
    }
}

fn run_worker(mut engine: Box<dyn AsrEngine>, receiver: Receiver<AsrMsg>, shared: &Shared) {
    let chunk = engine.chunk_samples().max(1);
    let mut pending: Vec<f32> = Vec::new();
    // Whether the engine has heard audio since its last reset.
    let mut heard = false;
    let mut text = String::new();
    loop {
        let message = match receiver.recv_timeout(POLL) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) if !shared.should_stop() => continue,
            Err(_) => return,
        };
        if shared.should_stop() {
            return;
        }
        let result: Result<()> = (|| {
            match message {
                AsrMsg::Audio { turn_id, samples } => {
                    pending.extend(samples);
                    let mut offset = 0;
                    while pending.len() - offset >= chunk && !shared.should_stop() {
                        let updated = engine.feed(&pending[offset..offset + chunk])?;
                        offset += chunk;
                        heard = true;
                        if updated != text {
                            text = updated;
                            shared.events.partial(turn_id, &text);
                        }
                    }
                    pending.drain(..offset);
                }
                AsrMsg::EndTurn(turn_id) => {
                    if heard || !pending.is_empty() {
                        text = engine.flush(&pending)?;
                    }
                    if shared.should_stop() {
                        return Ok(());
                    }
                    shared.events.final_text(turn_id, &text);
                    engine.reset();
                    (pending, heard, text) = (Vec::new(), false, String::new());
                }
                AsrMsg::CancelTurn(_) => {
                    engine.reset();
                    (pending, heard, text) = (Vec::new(), false, String::new());
                }
            }
            Ok(())
        })();
        if let Err(err) = result {
            shared.fail(&err.to_string());
            return;
        }
    }
}

/// A session thread and a way to wait for it with a deadline.
struct SessionThread {
    handle: JoinHandle<()>,
    /// Disconnects when the thread ends, however it ends.
    exited: Receiver<()>,
}

impl SessionThread {
    fn spawn(name: &str, shared: Arc<Shared>, work: impl FnOnce(&Shared) + Send + 'static) -> Result<Self> {
        let (exit_signal, exited) = channel::<()>();
        let handle = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let _guard = ExitGuard { shared: shared.clone(), _exit_signal: exit_signal };
                work(&shared);
            })
            .map_err(|err| anyhow!("Could not start voice input: {err}"))?;
        Ok(Self { handle, exited })
    }

    /// Waits until `deadline`; a thread still running then is left to finish by itself.
    fn join_by(self, deadline: Instant) {
        let wait = deadline.saturating_duration_since(Instant::now());
        if !matches!(self.exited.recv_timeout(wait), Err(RecvTimeoutError::Timeout)) {
            let _ = self.handle.join();
        }
    }
}

/// Reports a panic in a session thread as the session's error.
struct ExitGuard {
    shared: Arc<Shared>,
    _exit_signal: Sender<()>,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.shared.fail("Voice input stopped unexpectedly.");
        }
    }
}

/// The turns the caller opened, to reject calls out of order before they reach the threads.
#[derive(Default)]
struct TurnLedger {
    last: Option<u64>,
    open: Option<u64>,
}

/// One open microphone and one loaded ASR model, producing one transcript per turn.
///
/// A turn that reaches [`MAX_TURN`] (120 s) is ended automatically: its `final_text` is
/// sent then, the rest of the turn's audio is discarded, and the caller's later
/// `end_turn` or `cancel_turn` for it succeeds without another event.
pub(crate) struct VoiceSession {
    control: Sender<CaptureMsg>,
    shared: Arc<Shared>,
    ledger: Mutex<TurnLedger>,
    capture: Option<SessionThread>,
    worker: Option<SessionThread>,
}

impl VoiceSession {
    /// Loads the ASR model and opens the microphone (`device`: `None` is the system
    /// default). Blocking; call from a blocking thread. The model loads first, as in
    /// Transcribe, so the microphone is not open while nothing can listen.
    pub(crate) fn open(models_dir: &Path, device: Option<&str>, events: Arc<dyn VoiceEvents>) -> Result<Self> {
        let engine = ParakeetEngine(load_model(models_dir, 1)?);
        let device = device.map(str::to_string);
        Self::start(Box::new(engine), events, Limits::default(), move |sender| {
            let stream = InputStream::open(device.as_deref(), sender)?;
            Ok(Input {
                source_rate: stream.source_rate(),
                take_error: Box::new(move || stream.take_error()),
                silent_timeout: Some(SILENT_DEVICE_TIMEOUT),
            })
        })
    }

    /// Starts the threads. `open_input` runs on the capture thread and gets the sender
    /// its audio blocks must go to.
    fn start(
        engine: Box<dyn AsrEngine>,
        events: Arc<dyn VoiceEvents>,
        limits: Limits,
        open_input: impl FnOnce(Sender<CaptureMsg>) -> Result<Input> + Send + 'static,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            events: events.clone(),
            failure: Mutex::new(None),
            stopping: AtomicBool::new(false),
            next_turn: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });
        let (control, capture_receiver) = channel::<CaptureMsg>();
        let (asr_sender, asr_receiver) = sync_channel::<AsrMsg>(ASR_QUEUE_MESSAGES);
        let (ready_sender, ready) = channel::<Result<()>>();
        let input_sender = control.clone();
        let capture = SessionThread::spawn("assistant-voice-capture", shared.clone(), move |shared| {
            let input = match open_input(input_sender) {
                Ok(input) => input,
                Err(err) => {
                    let _ = ready_sender.send(Err(err));
                    return;
                }
            };
            let stage = CaptureStage::new(input.source_rate, limits, asr_sender, shared.events.clone(), shared.next_turn.clone());
            let _ = ready_sender.send(Ok(()));
            run_capture(input, capture_receiver, stage, shared);
        })?;
        let mut session = Self { control, shared: shared.clone(), ledger: Mutex::default(), capture: Some(capture), worker: None };
        match ready.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err(anyhow!("The microphone could not be opened.")),
        }
        session.worker = Some(SessionThread::spawn("assistant-voice-asr", shared, move |shared| {
            run_worker(engine, asr_receiver, shared)
        })?);
        Ok(session)
    }

    /// Starts forwarding microphone audio, and the pre-roll, as turn `turn_id`. Ids must
    /// increase, and the previous turn must have been ended or cancelled.
    pub(crate) fn begin_turn(&self, turn_id: u64) -> Result<()> {
        self.ensure_alive()?;
        let mut ledger = self.ledger.lock().map_err(|_| anyhow!("Voice input state lock poisoned"))?;
        if let Some(open) = ledger.open {
            return Err(anyhow!("Voice turn {open} is still open."));
        }
        if ledger.last.is_some_and(|last| turn_id <= last) {
            return Err(anyhow!("Voice turn ids must increase."));
        }
        self.send(CaptureMsg::Begin(turn_id))?;
        *ledger = TurnLedger { last: Some(turn_id), open: Some(turn_id) };
        Ok(())
    }

    /// Ends the open turn: every block captured before this call belongs to it, and
    /// `final_text` follows once the model has finished it.
    pub(crate) fn end_turn(&self, turn_id: u64) -> Result<()> {
        self.close_turn(turn_id, CaptureMsg::End(turn_id))
    }

    /// Discards the open turn; it gets no `final_text`.
    pub(crate) fn cancel_turn(&self, turn_id: u64) -> Result<()> {
        self.close_turn(turn_id, CaptureMsg::Cancel(turn_id))
    }

    /// A new turn id for [`Self::begin_turn`]. Ids increase across push-to-talk and
    /// automatic turns.
    pub(crate) fn next_turn_id(&self) -> u64 {
        self.shared.next_turn.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Switches automatic turn-taking on (with `decider`) or off (`None`), and sets the
    /// pre-roll kept before a turn. The detector starts turns only after some speech, so it
    /// needs a longer pre-roll than push-to-talk.
    pub(crate) fn set_auto(&self, decider: Option<Box<dyn TurnDecider>>, pre_roll: Duration) -> Result<()> {
        self.ensure_alive()?;
        let samples = (pre_roll.as_secs_f32() * SAMPLE_RATE as f32) as usize;
        self.send(CaptureMsg::SetAuto(decider, samples))
    }

    /// Pauses or resumes an installed detector (no reload). Returns an error if none is.
    pub(crate) fn pause_auto(&self, paused: bool) -> Result<()> {
        self.ensure_alive()?;
        self.send(CaptureMsg::PauseAuto(paused))
    }


    /// Stops capture and the worker and waits for them, at most [`JOIN_TIMEOUT`]. Queued
    /// turns are abandoned without `final_text`. Dropping the session does the same.
    pub(crate) fn close(mut self) {
        self.shutdown();
    }

    fn close_turn(&self, turn_id: u64, message: CaptureMsg) -> Result<()> {
        self.ensure_alive()?;
        let mut ledger = self.ledger.lock().map_err(|_| anyhow!("Voice input state lock poisoned"))?;
        if ledger.open != Some(turn_id) {
            return Err(anyhow!("Voice turn {turn_id} is not open."));
        }
        self.send(message)?;
        ledger.open = None;
        Ok(())
    }

    fn ensure_alive(&self) -> Result<()> {
        match self.shared.failure() {
            Some(message) => Err(anyhow!("Voice input stopped: {message}")),
            None => Ok(()),
        }
    }

    fn send(&self, message: CaptureMsg) -> Result<()> {
        self.control.send(message).map_err(|_| anyhow!("Voice input has stopped."))
    }

    fn shutdown(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        let _ = self.control.send(CaptureMsg::Close);
        let deadline = Instant::now() + JOIN_TIMEOUT;
        for thread in [self.capture.take(), self.worker.take()].into_iter().flatten() {
            thread.join_by(deadline);
        }
    }
}

impl Drop for VoiceSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;

    #[derive(Debug, Clone, PartialEq)]
    enum Event {
        Partial(u64, String),
        Final(u64, String),
        Error(String),
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<(Instant, Event)>>,
        levels: Mutex<Vec<f32>>,
    }

    impl VoiceEvents for Recorder {
        fn level(&self, rms: f32) {
            self.levels.lock().unwrap().push(rms);
        }
        fn partial(&self, turn_id: u64, text: &str) {
            self.events.lock().unwrap().push((Instant::now(), Event::Partial(turn_id, text.to_string())));
        }
        fn final_text(&self, turn_id: u64, text: &str) {
            self.events.lock().unwrap().push((Instant::now(), Event::Final(turn_id, text.to_string())));
        }
        fn error(&self, message: &str) {
            self.events.lock().unwrap().push((Instant::now(), Event::Error(message.to_string())));
        }
    }

    impl Recorder {
        fn finals(&self) -> Vec<(u64, String)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, event)| match event {
                    Event::Final(id, text) => Some((*id, text.clone())),
                    _ => None,
                })
                .collect()
        }

        fn errors(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, event)| match event {
                    Event::Error(message) => Some(message.clone()),
                    _ => None,
                })
                .collect()
        }

        fn final_at(&self, turn_id: u64) -> Option<Instant> {
            self.events.lock().unwrap().iter().find_map(|(at, event)| match event {
                Event::Final(id, _) if *id == turn_id => Some(*at),
                _ => None,
            })
        }

        /// Waits until `done` holds, at most `limit`.
        fn wait_until(&self, limit: Duration, done: impl Fn(&Self) -> bool) -> bool {
            let started = Instant::now();
            while started.elapsed() < limit {
                if done(self) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            done(self)
        }

        fn wait_finals(&self, count: usize) -> Vec<(u64, String)> {
            assert!(self.wait_until(Duration::from_secs(10), |recorder| recorder.finals().len() >= count), "finals: {:?}", self.finals());
            self.finals()
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Feed(Vec<u32>),
        Reset,
    }

    /// Stands in for the model. Block `n` of the synthetic audio is filled with the value
    /// `n`; the engine "hears" each block number as the word `bn`. Like the real model it
    /// emits a chunk's words only after the next chunk, so the flush is needed for the
    /// last words.
    struct FakeEngine {
        chunk: usize,
        committed: String,
        held: String,
        last_value: u32,
        delay: Duration,
        fail_after: Option<usize>,
        fed: usize,
        log: Arc<Mutex<Vec<Call>>>,
    }

    impl FakeEngine {
        fn new(log: Arc<Mutex<Vec<Call>>>) -> Self {
            Self { chunk: 160, committed: String::new(), held: String::new(), last_value: 0, delay: Duration::ZERO, fail_after: None, fed: 0, log }
        }
    }

    impl AsrEngine for FakeEngine {
        fn chunk_samples(&self) -> usize {
            self.chunk
        }

        fn feed(&mut self, chunk: &[f32]) -> Result<String> {
            assert_eq!(chunk.len(), self.chunk);
            self.fed += 1;
            if self.fail_after.is_some_and(|limit| self.fed > limit) {
                return Err(anyhow!("fake model failure"));
            }
            std::thread::sleep(self.delay);
            let mut values = Vec::new();
            let committed = std::mem::take(&mut self.held);
            self.committed.push_str(&committed);
            for &sample in chunk {
                let value = sample.round() as u32;
                if value != 0 && value != self.last_value {
                    self.held.push_str(&format!(" b{value}"));
                    self.last_value = value;
                    values.push(value);
                }
            }
            self.log.lock().unwrap().push(Call::Feed(values));
            Ok(join_words(std::iter::once(self.committed.as_str())))
        }

        fn reset(&mut self) {
            self.committed.clear();
            self.held.clear();
            self.last_value = 0;
            self.log.lock().unwrap().push(Call::Reset);
        }
    }

    /// A gate the fake microphone waits at, to hold the capture thread while a test
    /// queues blocks and boundaries behind it.
    #[derive(Default)]
    struct Gate {
        closed: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn set(&self, closed: bool) {
            *self.closed.lock().unwrap() = closed;
            self.changed.notify_all();
        }

        fn pass(&self) {
            let mut closed = self.closed.lock().unwrap();
            while *closed {
                closed = self.changed.wait(closed).unwrap();
            }
        }
    }

    struct Harness {
        session: VoiceSession,
        recorder: Arc<Recorder>,
        log: Arc<Mutex<Vec<Call>>>,
        gate: Arc<Gate>,
        microphone_error: Arc<Mutex<Option<String>>>,
        next_block: u32,
    }

    const BLOCK: usize = 100;

    impl Harness {
        fn new(limits: Limits, configure: impl FnOnce(&mut FakeEngine)) -> Self {
            let log = Arc::new(Mutex::new(Vec::new()));
            let mut engine = FakeEngine::new(log.clone());
            configure(&mut engine);
            let recorder = Arc::new(Recorder::default());
            let gate = Arc::new(Gate::default());
            let microphone_error = Arc::new(Mutex::new(None));
            let (input_gate, input_error) = (gate.clone(), microphone_error.clone());
            let session = VoiceSession::start(Box::new(engine), recorder.clone(), limits, move |_sender| {
                Ok(Input {
                    source_rate: SAMPLE_RATE,
                    take_error: Box::new(move || {
                        input_gate.pass();
                        input_error.lock().unwrap().take()
                    }),
                    silent_timeout: None,
                })
            })
            .unwrap();
            Self { session, recorder, log, gate, microphone_error, next_block: 1 }
        }

        fn no_pre_roll() -> Limits {
            Limits { pre_roll: 0, ..Limits::default() }
        }

        /// Sends `count` blocks as the microphone would and returns their numbers.
        fn speak(&mut self, count: u32) -> Vec<u32> {
            (0..count)
                .map(|_| {
                    let number = self.next_block;
                    self.next_block += 1;
                    // After a failure the capture thread is gone; the block is simply lost.
                    let _ = self.session.control.send(CaptureMsg::Block(vec![number as f32; BLOCK]));
                    number
                })
                .collect()
        }
    }

    fn words(blocks: &[u32]) -> String {
        blocks.iter().map(|block| format!("b{block}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn blocks_queued_before_end_turn_belong_to_that_turn() {
        let mut harness = Harness::new(Harness::no_pre_roll(), |_| {});
        // Hold the capture thread, so every block and boundary below is still queued
        // in the capture stage when `end_turn` is called.
        harness.gate.set(true);
        harness.session.begin_turn(1).unwrap();
        let first = harness.speak(40);
        harness.session.end_turn(1).unwrap();
        harness.session.begin_turn(2).unwrap();
        let second = harness.speak(5);
        harness.session.end_turn(2).unwrap();
        harness.gate.set(false);
        assert_eq!(harness.recorder.wait_finals(2), vec![(1, words(&first)), (2, words(&second))]);
    }

    /// Resampled input: the resampler's tail must reach the turn before its boundary.
    #[test]
    fn end_of_turn_flushes_the_resampler_before_the_boundary() {
        let (sender, receiver) = sync_channel(ASR_QUEUE_MESSAGES);
        let mut stage = CaptureStage::new(48_000, Harness::no_pre_roll(), sender, Arc::new(Recorder::default()), Arc::default());
        stage.control(CaptureMsg::Begin(7)).unwrap();
        for _ in 0..25 {
            stage.block(&[0.25; 441]).unwrap();
        }
        stage.control(CaptureMsg::End(7)).unwrap();
        stage.block(&[0.5; 4800]).unwrap();
        let messages: Vec<AsrMsg> = receiver.try_iter().collect();
        assert_eq!(messages.last(), Some(&AsrMsg::EndTurn(7)));
        let forwarded: usize = messages
            .iter()
            .map(|message| match message {
                AsrMsg::Audio { turn_id: 7, samples } => samples.len(),
                AsrMsg::EndTurn(7) => 0,
                other => panic!("unexpected {other:?}"),
            })
            .sum();
        // 25 * 441 samples at 48 kHz are 3675 at 16 kHz, all of them in the turn.
        assert_eq!(forwarded, 3_675);
    }

    /// A detector that reports scripted events after given blocks.
    struct ScriptedDecider {
        block: usize,
        script: Vec<(usize, AutoTurnEvent)>,
    }

    impl TurnDecider for ScriptedDecider {
        fn push(&mut self, _samples: &[f32]) -> Result<Vec<AutoTurnEvent>> {
            let block = self.block;
            self.block += 1;
            Ok(self.script.iter().filter(|(at, _)| *at == block).map(|(_, event)| *event).collect())
        }

        fn reset(&mut self) {}
    }

    /// Codex F1: automatic boundaries fall exactly at the blocks the detector judged.
    #[test]
    fn automatic_turns_end_exactly_at_the_judged_block() {
        let (sender, receiver) = sync_channel(ASR_QUEUE_MESSAGES);
        let mut stage = CaptureStage::new(16_000, Harness::no_pre_roll(), sender, Arc::new(Recorder::default()), Arc::default());
        let script = vec![(2, AutoTurnEvent::SpeechStarted), (5, AutoTurnEvent::EndOfTurn), (7, AutoTurnEvent::SpeechStarted)];
        stage.control(CaptureMsg::SetAuto(Some(Box::new(ScriptedDecider { block: 0, script })), 0)).unwrap();
        for n in 0..9 {
            stage.block(&[n as f32; 160]).unwrap();
        }
        let mut per_turn: Vec<(u64, Vec<f32>)> = Vec::new();
        let mut ended = Vec::new();
        for message in receiver.try_iter() {
            match message {
                AsrMsg::Audio { turn_id, samples } => {
                    let values: Vec<f32> = samples.chunks(160).map(|chunk| chunk[0]).collect();
                    match per_turn.last_mut() {
                        Some((id, all)) if *id == turn_id => all.extend(values),
                        _ => per_turn.push((turn_id, values)),
                    }
                }
                AsrMsg::EndTurn(turn_id) => ended.push(turn_id),
                AsrMsg::CancelTurn(turn_id) => panic!("unexpected cancel {turn_id}"),
            }
        }
        // Turn 1 is blocks 2 to 5; turn 2 starts at block 7 and is still open.
        assert_eq!(per_turn[0], (1, vec![2.0, 3.0, 4.0, 5.0]));
        assert_eq!(ended, vec![1]);
        stage.control(CaptureMsg::SetAuto(None, 0)).unwrap();
    }

    #[test]
    fn audio_during_a_flush_goes_to_the_next_turn_after_reset() {
        let mut harness = Harness::new(Harness::no_pre_roll(), |engine| engine.delay = Duration::from_millis(15));
        harness.session.begin_turn(1).unwrap();
        let first = harness.speak(20);
        harness.session.end_turn(1).unwrap();
        // The next turn starts at once, while the worker is still flushing turn 1.
        harness.session.begin_turn(2).unwrap();
        let second = harness.speak(20);
        harness.session.end_turn(2).unwrap();
        assert_eq!(harness.recorder.wait_finals(2), vec![(1, words(&first)), (2, words(&second))]);

        let log = harness.log.lock().unwrap().clone();
        let first_reset = log.iter().position(|call| *call == Call::Reset).unwrap();
        for (index, call) in log.iter().enumerate() {
            if let Call::Feed(values) = call {
                assert!(
                    values.iter().all(|value| first.contains(value)) || values.iter().all(|value| second.contains(value)),
                    "a chunk mixed both turns: {values:?}"
                );
                if values.iter().any(|value| second.contains(value)) {
                    assert!(index > first_reset, "turn 2 audio reached the model before the reset");
                }
            }
        }
    }

    #[test]
    fn a_cancelled_turn_has_no_final_text() {
        let mut harness = Harness::new(Harness::no_pre_roll(), |_| {});
        harness.session.begin_turn(1).unwrap();
        harness.speak(10);
        harness.session.cancel_turn(1).unwrap();
        harness.session.begin_turn(2).unwrap();
        let second = harness.speak(4);
        harness.session.end_turn(2).unwrap();
        assert_eq!(harness.recorder.wait_finals(1), vec![(2, words(&second))]);
        assert!(harness.recorder.finals().iter().all(|(id, _)| *id != 1));
        assert!(harness.session.end_turn(1).is_err(), "a cancelled turn cannot be ended");
    }

    #[test]
    fn audio_with_no_open_turn_is_discarded() {
        let mut harness = Harness::new(Harness::no_pre_roll(), |_| {});
        harness.speak(10);
        harness.session.begin_turn(1).unwrap();
        let first = harness.speak(3);
        harness.session.end_turn(1).unwrap();
        harness.speak(10);
        harness.session.begin_turn(2).unwrap();
        let second = harness.speak(3);
        harness.session.end_turn(2).unwrap();
        harness.speak(5);
        assert_eq!(harness.recorder.wait_finals(2), vec![(1, words(&first)), (2, words(&second))]);
    }

    #[test]
    fn the_pre_roll_keeps_only_the_audio_just_before_the_turn() {
        let mut harness = Harness::new(Limits { pre_roll: 2 * BLOCK, ..Limits::default() }, |_| {});
        let idle = harness.speak(6);
        harness.session.begin_turn(1).unwrap();
        let spoken = harness.speak(3);
        harness.session.end_turn(1).unwrap();
        // Right after a turn the pre-roll starts empty: nothing of turn 1 is repeated.
        let between = harness.speak(1);
        harness.session.begin_turn(2).unwrap();
        let next = harness.speak(2);
        harness.session.end_turn(2).unwrap();
        let expected_first: Vec<u32> = idle[4..].iter().chain(&spoken).copied().collect();
        let expected_second: Vec<u32> = between.iter().chain(&next).copied().collect();
        assert_eq!(harness.recorder.wait_finals(2), vec![(1, words(&expected_first)), (2, words(&expected_second))]);
    }

    #[test]
    fn final_text_comes_once_per_turn_in_order() {
        let mut harness = Harness::new(Harness::no_pre_roll(), |engine| engine.delay = Duration::from_millis(2));
        let mut expected = Vec::new();
        for turn_id in 1..=6u64 {
            harness.session.begin_turn(turn_id * 10).unwrap();
            // Turn 30 is a press and release with no audio at all.
            let blocks = if turn_id == 3 { Vec::new() } else { harness.speak(turn_id as u32 * 2) };
            harness.session.end_turn(turn_id * 10).unwrap();
            expected.push((turn_id * 10, words(&blocks)));
        }
        assert_eq!(harness.recorder.wait_finals(6), expected);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(harness.recorder.finals().len(), 6, "a turn was reported twice");
        let partials_in_order = harness.recorder.events.lock().unwrap().iter().all(|(_, event)| match event {
            Event::Partial(id, text) => !text.is_empty() && *id % 10 == 0,
            _ => true,
        });
        assert!(partials_in_order);
        assert!(!harness.recorder.levels.lock().unwrap().is_empty());
    }

    #[test]
    fn calls_out_of_order_are_rejected() {
        let harness = Harness::new(Harness::no_pre_roll(), |_| {});
        assert!(harness.session.end_turn(1).is_err());
        harness.session.begin_turn(5).unwrap();
        assert!(harness.session.begin_turn(6).is_err(), "a turn is already open");
        assert!(harness.session.end_turn(4).is_err());
        harness.session.end_turn(5).unwrap();
        assert!(harness.session.begin_turn(5).is_err(), "ids must increase");
        harness.session.begin_turn(6).unwrap();
    }

    #[test]
    fn a_long_turn_ends_itself_at_the_limit() {
        let mut harness = Harness::new(Limits { pre_roll: 0, max_turn: 10 * BLOCK }, |_| {});
        harness.session.begin_turn(1).unwrap();
        let blocks = harness.speak(15);
        assert_eq!(harness.recorder.wait_finals(1), vec![(1, words(&blocks[..10]))]);
        // The release after the limit is accepted and reports nothing more.
        harness.session.end_turn(1).unwrap();
        harness.session.begin_turn(2).unwrap();
        let next = harness.speak(2);
        harness.session.end_turn(2).unwrap();
        assert_eq!(harness.recorder.wait_finals(2), vec![(1, words(&blocks[..10])), (2, words(&next))]);
    }

    #[test]
    fn a_model_error_is_reported_once_and_close_is_prompt() {
        let mut harness = Harness::new(Harness::no_pre_roll(), |engine| engine.fail_after = Some(3));
        harness.session.begin_turn(1).unwrap();
        harness.speak(20);
        harness.session.end_turn(1).unwrap();
        assert!(harness.recorder.wait_until(Duration::from_secs(5), |recorder| !recorder.errors().is_empty()));
        // The capture thread notices the failure too; nothing else may be reported.
        harness.speak(20);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(harness.recorder.errors(), vec!["fake model failure".to_string()]);
        assert!(harness.recorder.finals().is_empty());
        assert!(harness.session.begin_turn(2).is_err());
        let started = Instant::now();
        harness.session.close();
        assert!(started.elapsed() < Duration::from_secs(1), "close took {:?}", started.elapsed());
    }

    #[test]
    fn a_microphone_error_is_reported_once_and_close_is_prompt() {
        let mut harness = Harness::new(Harness::no_pre_roll(), |_| {});
        harness.session.begin_turn(1).unwrap();
        harness.speak(3);
        *harness.microphone_error.lock().unwrap() = Some("device unplugged".to_string());
        assert!(harness.recorder.wait_until(Duration::from_secs(5), |recorder| !recorder.errors().is_empty()));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(harness.recorder.errors(), vec!["The microphone stopped working: device unplugged".to_string()]);
        assert!(harness.session.end_turn(1).is_err());
        let started = Instant::now();
        drop(harness.session);
        assert!(started.elapsed() < Duration::from_secs(1), "drop took {:?}", started.elapsed());
    }

    #[test]
    fn a_silent_microphone_is_an_error() {
        let recorder = Arc::new(Recorder::default());
        let session = VoiceSession::start(Box::new(FakeEngine::new(Arc::default())), recorder.clone(), Limits::default(), |_| {
            Ok(Input { source_rate: SAMPLE_RATE, take_error: Box::new(|| None), silent_timeout: Some(Duration::from_millis(100)) })
        })
        .unwrap();
        assert!(recorder.wait_until(Duration::from_secs(5), |recorder| recorder.errors().len() == 1));
        assert!(recorder.errors()[0].contains("No audio is arriving"));
        assert!(session.begin_turn(1).is_err());
        session.close();
    }

    #[test]
    fn a_microphone_that_cannot_open_fails_open() {
        let result = VoiceSession::start(Box::new(FakeEngine::new(Arc::default())), Arc::new(Recorder::default()), Limits::default(), |_| {
            Err(anyhow!("No microphone was found."))
        });
        assert_eq!(result.err().map(|err| err.to_string()), Some("No microphone was found.".to_string()));
    }

    #[test]
    fn close_does_not_wait_for_a_busy_model() {
        let mut harness = Harness::new(Harness::no_pre_roll(), |engine| engine.delay = Duration::from_millis(30));
        harness.session.begin_turn(1).unwrap();
        harness.speak(400);
        harness.session.end_turn(1).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        harness.session.close();
        assert!(started.elapsed() < Duration::from_secs(1), "close took {:?}", started.elapsed());
        assert!(harness.recorder.finals().is_empty(), "a queued turn finished after close");
        assert!(harness.recorder.errors().is_empty(), "close is not an error");
    }

    /// Plan §7.3: two spoken clips through one loaded model, as if from the microphone.
    /// The clips are made with macOS `say`, or read from `VOICEREADER_ASR_CLIP_A` and
    /// `VOICEREADER_ASR_CLIP_B` (WAV files of the two sentences below, any rate), for
    /// example on Windows. Run with:
    /// `VOICEREADER_ASR_MODELS_DIR=~/Library/Application\ Support/com.voicereader.desktop/data/models
    ///  cargo test --release --features build-base --lib -- --ignored --nocapture assistant_voice::tests::asr_turn_experiment`
    #[test]
    #[ignore = "needs the downloaded transcription models, and macOS `say` or recorded clips"]
    fn asr_turn_experiment() {
        const SENTENCE_A: &str = "The quick brown fox jumps over the lazy dog beside the river bank.";
        const SENTENCE_B: &str = "Please remind me to call the dentist tomorrow morning at nine.";
        let models_dir = std::path::PathBuf::from(std::env::var("VOICEREADER_ASR_MODELS_DIR").expect("set VOICEREADER_ASR_MODELS_DIR"));
        let max_speakers = std::env::var("VOICEREADER_ASR_MAX_SPEAKERS").ok().and_then(|value| value.parse().ok()).unwrap_or(1);
        let directory = tempfile::tempdir().unwrap();
        let clip = |name: &str, text: &str| -> Vec<f32> {
            let wav = match std::env::var(format!("VOICEREADER_ASR_CLIP_{}", name.to_uppercase())) {
                Ok(path) => std::path::PathBuf::from(path),
                Err(_) => {
                    let aiff = directory.path().join(format!("{name}.aiff"));
                    let wav = directory.path().join(format!("{name}.wav"));
                    assert!(std::process::Command::new("say").arg("-o").arg(&aiff).arg(text).status().unwrap().success());
                    assert!(std::process::Command::new("afconvert")
                        .args(["-f", "WAVE", "-d", "LEI16@16000"])
                        .arg(&aiff)
                        .arg(&wav)
                        .status()
                        .unwrap()
                        .success());
                    wav
                }
            };
            crate::audio8_model::load_wav_mono(&std::fs::read(wav).unwrap(), SAMPLE_RATE).unwrap()
        };
        let a = clip("a", SENTENCE_A);
        let b = clip("b", SENTENCE_B);
        println!("clip A {:.2} s, clip B {:.2} s", a.len() as f32 / 16_000.0, b.len() as f32 / 16_000.0);

        let loading = Instant::now();
        let engine = ParakeetEngine(load_model(&models_dir, max_speakers).unwrap());
        println!("model loaded in {:.2?} (max speakers {max_speakers})", loading.elapsed());
        let recorder = Arc::new(Recorder::default());
        let session = VoiceSession::start(Box::new(engine), recorder.clone(), Limits::default(), |_| {
            Ok(Input { source_rate: SAMPLE_RATE, take_error: Box::new(|| None), silent_timeout: None })
        })
        .unwrap();
        // 20 ms blocks at real-time pace, as a microphone delivers them.
        let play = |samples: &[f32]| {
            let started = Instant::now();
            for (index, block) in samples.chunks(320).enumerate() {
                session.control.send(CaptureMsg::Block(block.to_vec())).unwrap();
                let due = started + Duration::from_millis(20 * (index as u64 + 1));
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
            }
        };
        let quiet = vec![0.0f32; 8_000];
        let mut ended = Vec::new();

        // A microphone hears the room before the press; the pre-roll passes some of it on.
        play(&quiet);
        // (a) A and B back to back as one turn.
        session.begin_turn(1).unwrap();
        play(&a);
        play(&b);
        ended.push((1, Instant::now()));
        session.end_turn(1).unwrap();
        recorder.wait_finals(1);
        play(&quiet);
        // (b) A, end of turn, a pause, then B as a new turn.
        session.begin_turn(2).unwrap();
        play(&a);
        ended.push((2, Instant::now()));
        session.end_turn(2).unwrap();
        recorder.wait_finals(2);
        play(&quiet);
        session.begin_turn(3).unwrap();
        play(&b);
        ended.push((3, Instant::now()));
        session.end_turn(3).unwrap();
        recorder.wait_finals(3);
        play(&quiet);
        // (c) B starts while A is still being flushed.
        session.begin_turn(4).unwrap();
        play(&a);
        ended.push((4, Instant::now()));
        session.end_turn(4).unwrap();
        session.begin_turn(5).unwrap();
        play(&b);
        ended.push((5, Instant::now()));
        session.end_turn(5).unwrap();
        let finals = recorder.wait_finals(5);
        session.close();

        let mut latencies = Vec::new();
        for ((turn_id, text), (ended_id, ended_at)) in finals.iter().zip(&ended) {
            assert_eq!(turn_id, ended_id);
            let latency = recorder.final_at(*turn_id).unwrap().duration_since(*ended_at);
            latencies.push(latency);
            println!("turn {turn_id}: {latency:>8.0?} end-of-turn to final: {text}");
        }
        let normalized = |text: &str| -> Vec<String> {
            text.split_whitespace()
                .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
                .filter(|word| !word.is_empty())
                .collect()
        };
        let text_of = |turn_id: u64| normalized(&finals.iter().find(|(id, _)| *id == turn_id).unwrap().1);
        // Words the model writes one way only ("river bank" can come back as "riverbank",
        // "nine" as "9"), so the checks do not depend on its spelling choices.
        let only_a = ["quick", "brown", "fox", "lazy", "dog"];
        let only_b = ["remind", "dentist", "tomorrow", "morning"];
        let has = |words: &[String], wanted: &str| words.iter().any(|word| word == wanted);
        let a_ends = |words: &[String]| words.last().is_some_and(|word| word.ends_with("bank"));
        let b_ends = |words: &[String]| words.last().is_some_and(|word| word == "nine" || word == "9");
        let both = text_of(1);
        assert!(only_a.iter().chain(&only_b).all(|word| has(&both, word)) && b_ends(&both), "case (a) lost words: {both:?}");
        for turn_id in [2, 4] {
            let words = text_of(turn_id);
            assert!(a_ends(&words), "turn {turn_id} lost A's trailing words: {words:?}");
            assert!(only_a.iter().all(|word| has(&words, word)), "turn {turn_id} lost words of A: {words:?}");
            assert!(only_b.iter().all(|word| !has(&words, word)), "B leaked into turn {turn_id}: {words:?}");
        }
        for turn_id in [3, 5] {
            let words = text_of(turn_id);
            assert!(b_ends(&words), "turn {turn_id} lost B's trailing word: {words:?}");
            assert_eq!(words.first().map(String::as_str), Some("please"), "turn {turn_id} lost B's first word: {words:?}");
            assert!(only_b.iter().all(|word| has(&words, word)), "turn {turn_id} lost words of B: {words:?}");
            assert!(only_a.iter().all(|word| !has(&words, word)), "A leaked into turn {turn_id}: {words:?}");
        }
        // Nothing duplicated: every checked word appears at most once per turn.
        for turn_id in 1..=5 {
            let words = text_of(turn_id);
            for word in only_a.iter().chain(&only_b) {
                assert!(words.iter().filter(|candidate| candidate == word).count() <= 1, "turn {turn_id} repeats {word}: {words:?}");
            }
        }
        latencies.sort();
        println!("end-of-turn to final text: min {:.0?}, median {:.0?}, max {:.0?}", latencies[0], latencies[latencies.len() / 2], latencies[latencies.len() - 1]);
    }
}
