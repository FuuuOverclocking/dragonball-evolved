//! The i8042 keyboard controller, kept for its reset line: writing 0xfe to
//! its command port is how the guest reboots.

use std::sync::{Mutex, PoisonError};

use tokio::sync::mpsc;

use crate::bus::BusDevice;
use crate::machine::MachineEvent;
use crate::space::Pio;

/// The data port's offset within the controller's range.
pub const DATA: u16 = 0;
/// The status port on reads and the command port on writes.
pub const COMMAND: u16 = 4;

const CMD_READ_CTR: u8 = 0x20;
const CMD_WRITE_CTR: u8 = 0x60;
const CMD_READ_OUTP: u8 = 0xd0;
const CMD_WRITE_OUTP: u8 = 0xd1;
const CMD_RESET_CPU: u8 = 0xfe;
const KBD_ACK: u8 = 0xfa;
const SB_OUT_DATA_AVAIL: u8 = 1;
const SB_COMMAND_DATA: u8 = 1 << 3;
const SB_KBD_ENABLED: u8 = 1 << 4;
const CB_KBD_INTERRUPT: u8 = 1;
const CB_POST_OK: u8 = 1 << 2;

/// The controller's registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I8042State {
    status: u8,
    control: u8,
    output_port: u8,
    /// The command whose data byte is expected next.
    command: u8,
    output: Option<u8>,
}

impl Default for I8042State {
    fn default() -> Self {
        Self {
            status: SB_KBD_ENABLED,
            control: CB_POST_OK | CB_KBD_INTERRUPT,
            output_port: 0,
            command: 0,
            output: None,
        }
    }
}

/// What a register write asks of the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I8042Effect {
    Reset,
}

/// Reads the register at `offset`; reading data consumes it.
pub fn read(mut state: I8042State, offset: u16) -> (I8042State, u8) {
    let value = match offset {
        COMMAND => state.status,
        DATA => {
            state.status &= !SB_OUT_DATA_AVAIL;
            state.output.take().unwrap_or(0)
        }
        _ => 0,
    };
    (state, value)
}

/// Writes `value` to the register at `offset`. A reset leaves the state as it
/// was and is returned as an effect instead.
pub fn write(mut state: I8042State, offset: u16, value: u8) -> (I8042State, Option<I8042Effect>) {
    match (offset, value) {
        (COMMAND, CMD_RESET_CPU) => return (state, Some(I8042Effect::Reset)),
        (COMMAND, CMD_READ_CTR) => state.set_output(state.control),
        (COMMAND, CMD_WRITE_CTR) => state.expect_data(CMD_WRITE_CTR, true),
        (COMMAND, CMD_READ_OUTP) => state.set_output(state.output_port),
        (COMMAND, CMD_WRITE_OUTP) => state.expect_data(CMD_WRITE_OUTP, false),
        (DATA, value) if state.status & SB_COMMAND_DATA != 0 => {
            match state.command {
                CMD_WRITE_CTR => state.control = value,
                CMD_WRITE_OUTP => state.output_port = value,
                _ => {}
            }
            state.status &= !SB_COMMAND_DATA;
        }
        // A command for the keyboard itself, which every keyboard acknowledges.
        (DATA, _) => state.set_output(KBD_ACK),
        _ => {}
    }
    (state, None)
}

impl I8042State {
    fn set_output(&mut self, value: u8) {
        self.output = Some(value);
        self.status |= SB_OUT_DATA_AVAIL;
    }

    fn expect_data(&mut self, command: u8, flush: bool) {
        if flush {
            self.output = None;
            self.status &= !SB_OUT_DATA_AVAIL;
        }
        self.command = command;
        self.status |= SB_COMMAND_DATA;
    }
}

/// The controller on the PIO bus. A reset becomes [`MachineEvent::GuestReset`].
#[derive(Debug)]
pub struct I8042Device {
    state: Mutex<I8042State>,
    events: mpsc::UnboundedSender<MachineEvent>,
}

impl I8042Device {
    pub fn new(events: mpsc::UnboundedSender<MachineEvent>) -> Self {
        Self {
            state: Mutex::new(I8042State::default()),
            events,
        }
    }

    fn apply<T>(&self, access: impl FnOnce(I8042State) -> (I8042State, T)) -> T {
        // The state is replaced whole, so a poisoned lock still holds a valid one.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (next, output) = access(*state);
        *state = next;
        output
    }
}

impl BusDevice<Pio> for I8042Device {
    fn read(&self, offset: u16, data: &mut [u8]) {
        if let [byte] = data {
            *byte = self.apply(|state| read(state, offset));
        }
    }

    fn write(&self, offset: u16, data: &[u8]) {
        let &[value] = data else {
            return;
        };
        if self.apply(|state| write(state, offset, value)) == Some(I8042Effect::Reset) {
            // Nobody listening means the machine is already going away.
            let _ = self.events.send(MachineEvent::GuestReset);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_output_ports_roundtrip() {
        let state = I8042State::default();
        let (state, _) = write(state, COMMAND, CMD_WRITE_CTR);
        assert_ne!(state.status & SB_COMMAND_DATA, 0);
        let (state, _) = write(state, DATA, 0x12);
        assert_eq!(state.status & SB_COMMAND_DATA, 0);
        let (state, _) = write(state, COMMAND, CMD_READ_CTR);
        let (state, value) = read(state, DATA);
        assert_eq!(value, 0x12);
        assert_eq!(state.status & SB_OUT_DATA_AVAIL, 0);

        let (state, _) = write(state, COMMAND, CMD_WRITE_OUTP);
        let (state, _) = write(state, DATA, 0x34);
        let (state, _) = write(state, COMMAND, CMD_READ_OUTP);
        let (state, status) = read(state, COMMAND);
        assert_ne!(status & SB_OUT_DATA_AVAIL, 0);
        let (_, value) = read(state, DATA);
        assert_eq!(value, 0x34);
    }

    #[test]
    fn direct_keyboard_commands_are_acknowledged() {
        let (state, effect) = write(I8042State::default(), DATA, 0xed);
        assert_eq!(effect, None);
        assert_ne!(state.status & SB_OUT_DATA_AVAIL, 0);
        let (state, value) = read(state, DATA);
        assert_eq!(value, KBD_ACK);
        assert_eq!(read(state, DATA).1, 0);
    }

    #[test]
    fn reset_is_an_effect_not_a_state_mutation() {
        let state = I8042State::default();
        let (next, effect) = write(state, COMMAND, CMD_RESET_CPU);
        assert_eq!(next, state);
        assert_eq!(effect, Some(I8042Effect::Reset));
        assert_eq!(write(state, DATA, CMD_RESET_CPU).1, None);
    }

    #[test]
    fn device_reports_guest_reset_and_ignores_wide_accesses() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let device = I8042Device::new(events);
        device.write(COMMAND, &[CMD_RESET_CPU, 0]);
        let mut wide = [0xaa; 2];
        device.read(COMMAND, &mut wide);
        assert_eq!(wide, [0xaa; 2]);
        assert!(receiver.try_recv().is_err());

        let mut status = [0];
        device.read(COMMAND, &mut status);
        assert_eq!(status, [SB_KBD_ENABLED]);
        device.write(COMMAND, &[CMD_RESET_CPU]);
        assert_eq!(receiver.try_recv(), Ok(MachineEvent::GuestReset));
    }
}
