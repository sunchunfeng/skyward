use crate::waveform::{made, share, ticks, RATE, TICKS_RATE};
use anyhow::{anyhow, ensure, Context, Result};
use evdev::raw_stream::RawDevice;
use skyward_config::Strength;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// linux/input-event-codes.h
const EV_FF: u16 = 0x15;
const FF_PERIODIC: u16 = 0x51;
const FF_CUSTOM: u16 = 0x5d;

/// The ticks play at this share of the drive the driver gives samples at in
/// full, whatever the slider says: the 17 Ultra's own system plays them at
/// 3527 mV of the 8500 mV its device tree gives.
const TICKS_STRENGTH: f32 = 0.415;
/// How long the ticks stay loaded once their samples have run out, as the
/// phone's own system leaves them: the motor comes to rest on its own
/// before taking them away brakes it.
const TICKS_REST: Duration = Duration::from_millis(40);

/// The vibration motor, played through the kernel's force-feedback
/// events. The driver fills its queue with a made click's samples as the
/// click is loaded, refilling it for no second play, and keeps the click
/// playing after its samples have run out, even when told to stop, until
/// the click is taken away. So each click is loaded as it is played, at the
/// strength the slider gives, and taken away once it has played out.
pub struct Haptics {
    dev: File,
    click: Click,
    ticks: Click,
    ticks_share: f32,
    ticks_rest: Duration,
    strength: Strength,
    /// The click playing, by its slot and its count, until it is taken
    /// away; a click taken away early by the next is not taken away again.
    playing: Arc<Mutex<Option<(i16, u64)>>>,
    clicks: u64,
}

/// What a click is.
enum Click {
    /// A waveform the driver keeps, by its number. The driver says how long
    /// it plays as it loads it.
    Kept(i16),
    /// Samples made at the motor's resonant frequency, which the driver
    /// plays as they are, at `rate` a second.
    Samples { data: Vec<i8>, rate: u32 },
    /// ODM RTP samples whose amplitude is scaled before each upload.
    Rtp { data: Vec<i8>, rate: u32 },
}

impl Haptics {
    /// The force-feedback device and the configured click. Loading a click
    /// fills the driver's queue with what it plays, so the click is loaded
    /// only as it is played.
    pub fn open(cfg: &skyward_config::Haptics) -> Result<Haptics> {
        let (path, dev) = motor()?;
        let hz = match cfg.resonance.as_deref().map(resonance).transpose() {
            Ok(Some(hz)) => hz,
            Ok(None) => 172.4,
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!("the motor's resonance is not read, so 172.4 Hz is used: {e:#}"));
                172.4
            }
        };
        let click = if let Some(rtp) = &cfg.click_rtp {
            eprintln!("haptics {}: click RTP {} at {} Hz", path.display(), rtp.path, rtp.rate);
            load_rtp(rtp)?
        } else { match cfg.click {
            Some(id) => {
                eprintln!("haptics {}: clicks with waveform {id}, ticks made for a motor swinging at {hz} Hz", path.display());
                Click::Kept(id)
            }
            None => {
                eprintln!("haptics {}: clicks and ticks made for a motor swinging at {hz} Hz", path.display());
                Click::Samples { data: made(hz), rate: RATE }
            }
        } };
        let (ticks, ticks_share, ticks_rest) = if let Some(rtp) = &cfg.ticks_rtp {
            eprintln!("haptics {}: screen-off RTP {} at {} Hz", path.display(), rtp.path, rtp.rate);
            (load_rtp(rtp)?, share(&cfg.strength, 0.5), Duration::ZERO)
        } else {
            (Click::Samples { data: ticks(hz), rate: TICKS_RATE }, TICKS_STRENGTH, TICKS_REST)
        };
        Ok(Haptics { dev, click, ticks, ticks_share, ticks_rest, strength: cfg.strength, playing: Arc::new(Mutex::new(None)), clicks: 0 })
    }

    /// One short click at the strength the slider's `position` gives, after
    /// taking away one still playing. Gives how long it plays.
    pub fn click(&mut self, position: f32) -> Result<Duration> {
        self.start(false, share(&self.strength, position))
    }

    /// Screen-off feedback: the configured RTP waveform, or the existing
    /// synthesized ticks on devices without one. Gives how long it plays.
    pub fn ticks(&mut self) -> Result<Duration> {
        self.start(true, self.ticks_share)
    }

    /// Play the ticks, or else the click, at `share` of its full strength,
    /// after taking away what still plays.
    fn start(&mut self, ticks: bool, share: f32) -> Result<Duration> {
        let mut playing = self.playing.lock().expect("no thread panics while it holds the click playing");
        if let Some((slot, _)) = playing.take() {
            erase(&self.dev, slot).context("take away the click still playing")?;
        }
        let effect = if ticks { &self.ticks } else { &self.click };
        let loaded = upload(&self.dev, effect, share).context("load the click")?;
        if let Err(error) = play(&mut self.dev, loaded.slot) {
            let _ = erase(&self.dev, loaded.slot);
            return Err(error).context("play the click");
        }
        self.clicks += 1;
        let this = (loaded.slot, self.clicks);
        *playing = Some(this);
        drop(playing);
        let dev = self.dev.try_clone().context("share the motor")?;
        let length = loaded.length + if ticks { self.ticks_rest } else { Duration::ZERO };
        // Timed from now: a thread may wait a while before it first runs.
        let until = Instant::now() + length;
        let playing = self.playing.clone();
        std::thread::spawn(move || {
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
            let mut playing = playing.lock().expect("no thread panics while it holds the click playing");
            if *playing == Some(this) {
                *playing = None;
                if let Err(e) = erase(&dev, this.0) {
                    eprintln!("take away the click: {e}");
                }
            }
        });
        Ok(length)
    }
}

/// Play the configured click once at the slider's `position`, or else the
/// ticks of the screen going off, saying how strongly and for how long, for
/// trying out a motor.
pub fn probe(cfg: &skyward_config::Haptics, position: f32, ticks: bool) -> Result<()> {
    let mut haptics = Haptics::open(cfg)?;
    let (what, strength, length) = if ticks {
        ("ticks", haptics.ticks_share, haptics.ticks()?)
    } else {
        ("click", share(&cfg.strength, position), haptics.click(position)?)
    };
    // Once it has played out, what played goes with the device as the
    // command ends.
    std::thread::sleep(length);
    println!("played the {what} at {strength:.3} of the full strength, {} ms long", length.as_millis());
    Ok(())
}

/// Bring the motor's driver up: put the firmware it asks the kernel's loader
/// for where the loader looks, then load the modules it is in. A driver the
/// phone's own init loaded already has spent its probe on the empty folder
/// its recovery leaves; putting the firmware there does not try it again, so
/// the device is unbound from its driver and bound back to it, which probes
/// it afresh with the file now in place.
pub fn bring_up(cfg: &skyward_config::Haptics, sources: &[String]) -> Result<()> {
    for file in &cfg.firmware {
        crate::wlan::place_firmware(file)?;
    }
    if !cfg.modules.is_empty() {
        let dirs = sources.iter().map(|s| skyward_modules::source_dir(s)).collect::<Result<Vec<_>>>()?;
        for path in skyward_modules::load(&dirs, &cfg.modules)? {
            skyward_sys::kmsg(&format!("loaded {} for the motor", path.display()))?;
        }
    }
    // Only a driver that asked for firmware and went without it has to be
    // prodded; one that probed cleanly made its device already.
    if !cfg.firmware.is_empty() && !any_motor() {
        reprobe(cfg)?;
    }
    Ok(())
}

/// Unbind each of the configured drivers from its device and bind it back,
/// so the driver probes the motor again now its firmware is where the loader
/// looks. The drivers are found by the names the kernel gives them under
/// /sys/bus/i2c/drivers, which drop the suffix a module's file name carries:
/// cs40l26-i2c is the driver cs40l26.
fn reprobe(cfg: &skyward_config::Haptics) -> Result<()> {
    for module in &cfg.modules {
        let driver = driver_name(module);
        let at = Path::new("/sys/bus/i2c/drivers").join(&driver);
        let Ok(listing) = fs::read_dir(&at) else { continue };
        let bound: Vec<_> = listing.flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().contains('-')).collect();
        for name in &bound {
            let name = name.to_string_lossy();
            let _ = fs::write(at.join("unbind"), name.as_ref());
            let _ = fs::write(at.join("bind"), name.as_ref());
            skyward_sys::kmsg(&format!("bound {driver} to {name} again so it probes the motor with its firmware"))?;
        }
    }
    // The device node and its force-feedback device appear as the probe
    // runs, which is not synchronous with the write above.
    let until = std::time::Instant::now() + PROBE_WAIT;
    while std::time::Instant::now() < until {
        if any_motor() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

/// The kernel's name for the driver a module is in: the module's own name
/// without the bus it sits on, as cs40l26-i2c is the driver cs40l26 and
/// cs40l26-core is none.
fn driver_name(module: &str) -> String {
    module.trim_end_matches("-core").trim_end_matches("-i2c").replace('_', "-")
}

/// Whether any input device takes periodic force-feedback effects yet.
fn any_motor() -> bool {
    evdev::raw_stream::enumerate().any(|(_, dev)| takes_periodic(&dev))
}

/// How long a driver may take to make its force-feedback device once bound
/// to the motor again.
const PROBE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// The first input device under /dev/input that takes periodic
/// force-feedback effects, opened for writing.
fn motor() -> Result<(PathBuf, File)> {
    for (path, dev) in evdev::raw_stream::enumerate() {
        if takes_periodic(&dev) {
            let file = OpenOptions::new().read(true).write(true).open(&path).with_context(|| format!("open {}", path.display()))?;
            return Ok((path, file));
        }
    }
    Err(anyhow!("no input device takes force-feedback effects"))
}

fn takes_periodic(dev: &RawDevice) -> bool {
    dev.supported_ff().is_some_and(|ff| ff.contains(evdev::FFEffectCode::FF_PERIODIC))
}

fn load_rtp(rtp: &skyward_config::Rtp) -> Result<Click> {
    let (data,rate) = crate::haptics_rtp::load(rtp)?;
    Ok(Click::Rtp { data,rate })
}
/// The motor's resonant frequency in hertz, from the file the driver says
/// it in, in tenths of a hertz.
fn resonance(file: &str) -> Result<f32> {
    let text = fs::read_to_string(file).with_context(|| format!("read {file}"))?;
    let tenths: u32 = text.split_whitespace().next().and_then(|n| n.parse().ok()).with_context(|| format!("{file} holds {text:?}"))?;
    ensure!(tenths > 0, "{file} says the motor swings at 0 Hz");
    Ok(tenths as f32 / 10.0)
}

/// A click as the driver loaded it: the slot the kernel keeps it in, and
/// how long it plays.
struct Loaded {
    slot: i16,
    length: Duration,
}

/// Samples handed to the driver to play as they are, as its
/// `custom_fifo_data` lays them out.
#[repr(C)]
struct Samples {
    /// Reserved effect-stream index; raw RTP playback uses zero.
    index: u32,
    length: u32,
    rate: u32,
    data: *const i8,
}

/// Load `click` at `share` of its full strength as a custom periodic
/// effect in a slot of its own. The driver drives the motor at the click's
/// highest voltage times the magnitude.
fn upload(dev: &File, click: &Click, share: f32) -> Result<Loaded> {
    // The driver tells the two kinds apart by the length in bytes of what
    // the effect points to, and loads nothing for any other length, though
    // the upload still succeeds. For a waveform it keeps it reads the
    // waveform's number from the first of three values and writes how long
    // it plays into the other two, in whole seconds and then milliseconds.
    let mut kept: [i16; 3] = [0; 3];
    let mut samples = Samples { index: 0, length: 0, rate: RATE, data: std::ptr::null() };
    // Keep the scaled allocation alive through EVIOCSFF. Full magnitude
    // avoids applying the user's strength twice on drivers that do honor it.
    let scaled = match click {
        Click::Rtp { data, .. } => Some(crate::haptics_rtp::scaled(data, share)),
        _ => None,
    };
    let (len, data) = match click {
        Click::Kept(id) => {
            kept[0] = *id;
            (std::mem::size_of_val(&kept), kept.as_mut_ptr())
        }
        Click::Samples { data, rate } | Click::Rtp { data, rate } => {
            samples.length = u32::try_from(data.len())?;
            samples.rate = *rate;
            samples.data = scaled.as_deref().unwrap_or(data).as_ptr();
            (std::mem::size_of::<Samples>(), (&mut samples as *mut Samples).cast::<i16>())
        }
    };
    let periodic = libc::ff_periodic_effect {
        waveform: FF_CUSTOM,
        period: 0,
        magnitude: (if matches!(click, Click::Rtp { .. }) { 1.0 } else { share.clamp(0.0, 1.0) } * f32::from(i16::MAX)).round() as i16,
        offset: 0,
        phase: 0,
        envelope: libc::ff_envelope { attack_length: 0, attack_level: 0, fade_length: 0, fade_level: 0 },
        custom_len: len as u32,
        custom_data: data,
    };
    let mut effect = libc::ff_effect {
        type_: FF_PERIODIC,
        id: -1,
        direction: 0,
        trigger: libc::ff_trigger { button: 0, interval: 0 },
        replay: libc::ff_replay { length: 0, delay: 0 },
        u: [0; 4],
    };
    // The union holds the periodic effect; it is exactly as large.
    const _: () = assert!(std::mem::size_of::<libc::ff_periodic_effect>() == std::mem::size_of::<[u64; 4]>());
    effect.u = unsafe { std::mem::transmute::<libc::ff_periodic_effect, [u64; 4]>(periodic) };
    // EVIOCSFF: _IOW('E', 0x80, struct ff_effect)
    let size = std::mem::size_of::<libc::ff_effect>() as u32;
    let request = (1u32 << 30) | (size << 16) | (u32::from(b'E') << 8) | 0x80;
    if unsafe { libc::ioctl(dev.as_raw_fd(), request as _, &mut effect) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let length = match click {
        Click::Kept(id) => {
            let [_, seconds, millis] = kept;
            let length = Duration::from_secs(u64::try_from(seconds)?) + Duration::from_millis(u64::try_from(millis)?);
            ensure!(!length.is_zero(), "the driver loaded waveform {id} without saying how long it plays");
            length
        }
        // The queue is full before the samples start playing.
        // Qualcomm's public effect-stream HAL waits the sample duration plus
        // one millisecond before removing it; rounding up avoids truncating
        // the final sample of an externally loaded RTP waveform.
        Click::Samples { data, rate } | Click::Rtp { data, rate } => Duration::from_millis((data.len() as u64 * 1000).div_ceil(u64::from(*rate)) + 1),
    };
    Ok(Loaded { slot: effect.id, length })
}

/// Start the loaded effect in slot `id`.
fn play(dev: &mut File, id: i16) -> io::Result<()> {
    let event = libc::input_event { time: libc::timeval { tv_sec: 0, tv_usec: 0 }, type_: EV_FF, code: id as u16, value: 1 };
    let bytes = unsafe { std::slice::from_raw_parts((&event as *const libc::input_event).cast::<u8>(), std::mem::size_of::<libc::input_event>()) };
    dev.write_all(bytes)
}

/// Take the effect in slot `id` away, which stops it for good.
fn erase(dev: &File, id: i16) -> io::Result<()> {
    // EVIOCRMFF: _IOW('E', 0x81, int)
    let request = (1u32 << 30) | ((std::mem::size_of::<libc::c_int>() as u32) << 16) | (u32::from(b'E') << 8) | 0x81;
    if unsafe { libc::ioctl(dev.as_raw_fd(), request as _, libc::c_int::from(id)) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
