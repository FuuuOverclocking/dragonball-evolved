use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::machine::MachineEvent;
use crate::machine::bus::BusDevice;

const DATA: u64 = 0;
const STATUS: u64 = 4;
const CMD_READ_CTR: u8 = 0x20;
const CMD_WRITE_CTR: u8 = 0x60;
const CMD_READ_OUTP: u8 = 0xd0;
const CMD_WRITE_OUTP: u8 = 0xd1;
const CMD_RESET_CPU: u8 = 0xfe;
const OUT_DATA_AVAIL: u8 = 1;
const COMMAND_DATA: u8 = 1 << 3;
const KBD_ENABLED: u8 = 1 << 4;
const KBD_INTERRUPT: u8 = 1;
const POST_OK: u8 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I8042State {
    status: u8,
    control: u8,
    output_port: u8,
    command: u8,
    output: Option<u8>,
}

impl Default for I8042State {
    fn default() -> Self {
        Self {
            status: KBD_ENABLED,
            control: POST_OK | KBD_INTERRUPT,
            output_port: 0,
            command: 0,
            output: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I8042Effect {
    Reset,
}

pub fn read(mut state: I8042State, offset: u64) -> (I8042State, u8) {
    let value = match offset {
        STATUS => state.status,
        DATA => {
            let value = state.output.take().unwrap_or(0);
            if state.output.is_none() {
                state.status &= !OUT_DATA_AVAIL;
            }
            value
        }
        _ => 0,
    };
    (state, value)
}

pub fn write(mut state: I8042State, offset: u64, value: u8) -> (I8042State, Option<I8042Effect>) {
    match (offset, value) {
        (STATUS, CMD_RESET_CPU) => return (state, Some(I8042Effect::Reset)),
        (STATUS, CMD_READ_CTR) => state.set_output(state.control),
        (STATUS, CMD_WRITE_CTR) => state.expect(CMD_WRITE_CTR, true),
        (STATUS, CMD_READ_OUTP) => state.set_output(state.output_port),
        (STATUS, CMD_WRITE_OUTP) => state.expect(CMD_WRITE_OUTP, false),
        (DATA, value) if state.status & COMMAND_DATA != 0 => {
            match state.command {
                CMD_WRITE_CTR => state.control = value,
                CMD_WRITE_OUTP => state.output_port = value,
                _ => {}
            }
            state.status &= !COMMAND_DATA;
        }
        (DATA, _) => state.set_output(0xfa),
        _ => {}
    }
    (state, None)
}

impl I8042State {
    fn set_output(&mut self, value: u8) {
        self.output = Some(value);
        self.status |= OUT_DATA_AVAIL;
    }

    fn expect(&mut self, command: u8, flush: bool) {
        if flush {
            self.output = None;
            self.status &= !OUT_DATA_AVAIL;
        }
        self.command = command;
        self.status |= COMMAND_DATA;
    }
}

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
}

impl BusDevice for I8042Device {
    fn read(&self, offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            return;
        }
        let mut state = self.state.lock().unwrap();
        let (next, value) = read(*state, offset);
        *state = next;
        data[0] = value;
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            return;
        }
        let effect = {
            let mut state = self.state.lock().unwrap();
            let (next, effect) = write(*state, offset, data[0]);
            *state = next;
            effect
        };
        if effect == Some(I8042Effect::Reset) {
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
        let (state, _) = write(state, STATUS, CMD_WRITE_CTR);
        assert_ne!(state.status & COMMAND_DATA, 0);
        let (state, _) = write(state, DATA, 0x12);
        let (state, _) = write(state, STATUS, CMD_READ_CTR);
        let (state, value) = read(state, DATA);
        assert_eq!(value, 0x12);
        assert_eq!(state.status & OUT_DATA_AVAIL, 0);

        let (state, _) = write(state, STATUS, CMD_WRITE_OUTP);
        let (state, _) = write(state, DATA, 0x34);
        let (state, _) = write(state, STATUS, CMD_READ_OUTP);
        let (_, value) = read(state, DATA);
        assert_eq!(value, 0x34);
    }

    #[test]
    fn direct_keyboard_commands_are_acknowledged() {
        let (state, effect) = write(I8042State::default(), DATA, 0xed);
        assert_eq!(effect, None);
        assert_ne!(state.status & OUT_DATA_AVAIL, 0);
        let (_, value) = read(state, DATA);
        assert_eq!(value, 0xfa);
    }

    #[test]
    fn reset_is_an_effect_not_a_state_mutation() {
        let state = I8042State::default();
        let (next, effect) = write(state, STATUS, CMD_RESET_CPU);
        assert_eq!(next, state);
        assert_eq!(effect, Some(I8042Effect::Reset));
    }

    #[tokio::test]
    async fn device_reports_guest_reset() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let device = I8042Device::new(events);
        device.write(STATUS, &[CMD_RESET_CPU]);
        assert!(matches!(
            receiver.recv().await,
            Some(MachineEvent::GuestReset)
        ));
    }
}
