//! The shared sound on Linux (0.52.0), through libpulse, which PulseAudio
//! and PipeWire (pipewire-pulse) both answer.
//!
//! A sink's monitor follows its mute and volume, so muting the speakers
//! would silence what is recorded too. To send the sound of this computer
//! without playing it here, RustInput adds a silent sink, `rustinput_sound`,
//! makes it the default (what plays moves there at once) and records its
//! monitor. The sink keeps the name of the output that was the default
//! before in its properties, so the speakers come back also after RustInput
//! stopped without putting them back itself ([`clean_up`], at the next
//! start).

use std::{cell::RefCell, rc::Rc};

use libpulse_binding::{
    callbacks::ListResult,
    context::{Context, FlagSet, State},
    def::BufferAttr,
    mainloop::standard::{IterateResult, Mainloop},
    operation::{Operation, State as OperationState},
    sample::{Format, Spec},
    stream::Direction,
};
use libpulse_simple_binding::Simple;

use super::wire::{CHANNELS, PCM_BYTES, RATE};

/// the silent sink; its monitor is `rustinput_sound.monitor`
pub(crate) const SINK: &str = "rustinput_sound";
/// in the arguments of the module of the sink: the output to put back
const RESTORE: &str = "rustinput.restore=";

fn spec() -> Spec {
    Spec {
        format: Format::S16le,
        channels: CHANNELS as u8,
        rate: RATE,
    }
}

/// a connection to the sound server for a few requests
struct Pulse {
    // the context goes before the loop it runs on
    context: Context,
    mainloop: Mainloop,
}

impl Pulse {
    fn connect() -> Result<Self, String> {
        let mainloop = Mainloop::new().ok_or("no loop for the sound server")?;
        let mut context =
            Context::new(&mainloop, "RustInput").ok_or("no connection to the sound server")?;
        context
            .connect(None, FlagSet::NOAUTOSPAWN, None)
            .map_err(|e| format!("the sound server: {e}"))?;
        let mut pulse = Self { context, mainloop };
        loop {
            match pulse.context.get_state() {
                State::Ready => return Ok(pulse),
                State::Failed | State::Terminated => {
                    return Err("the sound server refused the connection".into());
                }
                _ => pulse.turn()?,
            }
        }
    }

    fn turn(&mut self) -> Result<(), String> {
        match self.mainloop.iterate(true) {
            IterateResult::Success(_) => Ok(()),
            _ => Err("the sound server went away".into()),
        }
    }

    fn wait<F: ?Sized>(&mut self, operation: Operation<F>) -> Result<(), String> {
        while operation.get_state() == OperationState::Running {
            self.turn()?;
        }
        Ok(())
    }

    fn default_sink(&mut self) -> Result<Option<String>, String> {
        let found = Rc::new(RefCell::new(None));
        let into = Rc::clone(&found);
        let operation = self.context.introspect().get_server_info(move |info| {
            *into.borrow_mut() = info.default_sink_name.as_ref().map(|n| n.to_string());
        });
        self.wait(operation)?;
        Ok(found.take())
    }

    fn set_default_sink(&mut self, name: &str) -> Result<bool, String> {
        let done = Rc::new(RefCell::new(false));
        let into = Rc::clone(&done);
        let operation = self
            .context
            .set_default_sink(name, move |ok| *into.borrow_mut() = ok);
        self.wait(operation)?;
        Ok(done.take())
    }

    fn sink_exists(&mut self, name: &str) -> Result<bool, String> {
        let found = Rc::new(RefCell::new(false));
        let into = Rc::clone(&found);
        let operation = self
            .context
            .introspect()
            .get_sink_info_by_name(name, move |item| {
                if let ListResult::Item(_) = item {
                    *into.borrow_mut() = true;
                }
            });
        self.wait(operation)?;
        Ok(found.take())
    }

    /// the modules that made our sink: their index, and the output each
    /// says to put back
    fn ours(&mut self) -> Result<Vec<(u32, Option<String>)>, String> {
        let found = Rc::new(RefCell::new(Vec::new()));
        let into = Rc::clone(&found);
        let operation = self.context.introspect().get_module_info_list(move |item| {
            if let ListResult::Item(module) = item {
                let argument = module.argument.as_deref().unwrap_or("");
                if module.name.as_deref() == Some("module-null-sink")
                    && argument
                        .split_whitespace()
                        .any(|word| word == format!("sink_name={SINK}"))
                {
                    into.borrow_mut().push((module.index, restore_of(argument)));
                }
            }
        });
        self.wait(operation)?;
        Ok(found.take())
    }

    fn load_sink(&mut self, restore: &str) -> Result<u32, String> {
        let index = Rc::new(RefCell::new(u32::MAX));
        let into = Rc::clone(&index);
        // a name with a space or a quote would break the arguments
        let restore: String = restore
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
            .collect();
        let argument = format!(
            "sink_name={SINK} sink_properties=\"device.description=RustInput {RESTORE}{restore}\""
        );
        let operation =
            self.context
                .introspect()
                .load_module("module-null-sink", &argument, move |loaded| {
                    *into.borrow_mut() = loaded
                });
        self.wait(operation)?;
        match index.take() {
            u32::MAX => Err("the sound server would not add the sink of RustInput".into()),
            loaded => Ok(loaded),
        }
    }

    fn unload(&mut self, index: u32) -> Result<(), String> {
        let operation = self.context.introspect().unload_module(index, |_| {});
        self.wait(operation)
    }

    /// puts `restore` back as the default output, if it is still there,
    /// and removes the sink of `module`
    fn put_back(&mut self, module: u32, restore: Option<&str>) -> Result<(), String> {
        if self.default_sink()?.as_deref() == Some(SINK) {
            if let Some(restore) = restore.filter(|r| !r.is_empty()) {
                if self.sink_exists(restore)? {
                    self.set_default_sink(restore)?;
                }
            }
        }
        self.unload(module)
    }
}

/// the output named after `rustinput.restore=` in the arguments
fn restore_of(argument: &str) -> Option<String> {
    let at = argument.find(RESTORE)? + RESTORE.len();
    let name: String = argument[at..]
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Removes the sink of an earlier RustInput that stopped without removing
/// it (crash, `kill -9`), putting the output it replaced back.
pub(crate) fn clean_up() -> Result<(), String> {
    let mut pulse = Pulse::connect()?;
    for (module, restore) in pulse.ours()? {
        log::info!(
            "sound: putting back {} in place of the sink of RustInput",
            restore.as_deref().unwrap_or("the default output")
        );
        pulse.put_back(module, restore.as_deref())?;
    }
    Ok(())
}

/// The speakers of this computer moved to the silent sink while the sound
/// goes to another computer; put back when dropped.
pub(crate) struct Silenced {
    module: u32,
    restore: Option<String>,
}

/// moves what plays here to the silent sink, which is recorded instead
pub(crate) fn silence() -> Result<Silenced, String> {
    let mut pulse = Pulse::connect()?;
    for (module, restore) in pulse.ours()? {
        pulse.put_back(module, restore.as_deref())?;
    }
    let restore = pulse.default_sink()?.filter(|s| s != SINK);
    let module = pulse.load_sink(restore.as_deref().unwrap_or(""))?;
    if !pulse.set_default_sink(SINK)? {
        let _ = pulse.unload(module);
        return Err("the sound server would not make the sink of RustInput the default".into());
    }
    log::info!(
        "sound: the speakers ({}) are silent while the sound goes to another computer",
        restore.as_deref().unwrap_or("?")
    );
    Ok(Silenced { module, restore })
}

impl Drop for Silenced {
    fn drop(&mut self) {
        let result =
            Pulse::connect().and_then(|mut p| p.put_back(self.module, self.restore.as_deref()));
        match result {
            Ok(()) => log::info!(
                "sound: the speakers ({}) play again",
                self.restore.as_deref().unwrap_or("?")
            ),
            Err(e) => log::warn!("sound: the speakers could not be put back: {e}"),
        }
    }
}

/// the monitor of the output that plays now
pub(crate) fn default_monitor() -> Result<String, String> {
    let sink = Pulse::connect()?
        .default_sink()?
        .ok_or("the sound server has no output")?;
    Ok(format!("{sink}.monitor"))
}

/// What records the sound of this computer: the monitor of the silent
/// sink, with the speakers silenced, or the one of the speakers.
pub(crate) struct Capture {
    // the recorder goes before the speakers come back
    recorder: Recorder,
    _silenced: Option<Silenced>,
}

impl Capture {
    pub(crate) fn new(keep_speakers: bool) -> Result<Self, String> {
        if keep_speakers {
            return Ok(Self {
                recorder: Recorder::new(&default_monitor()?)?,
                _silenced: None,
            });
        }
        let silenced = silence()?;
        let recorder = Recorder::new(&format!("{SINK}.monitor"))?;
        Ok(Self {
            recorder,
            _silenced: Some(silenced),
        })
    }

    /// the next 5 ms; waits for them
    pub(crate) fn read(&mut self, into: &mut [u8; PCM_BYTES]) -> Result<(), String> {
        self.recorder.read(into)
    }
}

/// records 5 ms at a time from a monitor
pub(crate) struct Recorder(Simple);

impl Recorder {
    pub(crate) fn new(source: &str) -> Result<Self, String> {
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: PCM_BYTES as u32,
        };
        Simple::new(
            None,
            "RustInput",
            Direction::Record,
            Some(source),
            "the sound of this computer for another one",
            &spec(),
            None,
            Some(&attr),
        )
        .map(Self)
        .map_err(|e| format!("recording {source}: {e}"))
    }

    pub(crate) fn read(&mut self, into: &mut [u8; PCM_BYTES]) -> Result<(), String> {
        self.0.read(into).map_err(|e| format!("recording: {e}"))
    }
}

/// plays on the output of this computer
pub(crate) struct Player(Simple);

impl Player {
    pub(crate) fn new() -> Result<Self, String> {
        // about 20 ms in the server: little delay, and room for a late turn
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: (4 * PCM_BYTES) as u32,
            prebuf: (2 * PCM_BYTES) as u32,
            minreq: PCM_BYTES as u32,
            fragsize: u32::MAX,
        };
        Simple::new(
            None,
            "RustInput",
            Direction::Playback,
            None,
            "the sound of the other computers",
            &spec(),
            None,
            Some(&attr),
        )
        .map(Self)
        .map_err(|e| format!("playing: {e}"))
    }

    /// plays 5 ms; waits while the server holds enough
    pub(crate) fn write(&mut self, pcm: &[u8; PCM_BYTES]) -> Result<(), String> {
        self.0.write(pcm).map_err(|e| format!("playing: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_to_put_back() {
        let argument = format!(
            "sink_name={SINK} sink_properties=\"device.description=RustInput {RESTORE}alsa_output.usb-X.iec958-stereo\""
        );
        assert_eq!(
            restore_of(&argument).as_deref(),
            Some("alsa_output.usb-X.iec958-stereo")
        );
        assert_eq!(restore_of(&format!("sink_name={SINK}")), None);
        assert_eq!(restore_of(&format!("{RESTORE}\"")), None);
    }
}
