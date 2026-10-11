use std::os::fd::OwnedFd;

use crate::helpers::fd::{Fds, MergeFd, SplitFd};
use crate::{ApiError, ApiResult};

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HandoverState {}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LiveUpgradeState {
    #[cfg_attr(feature = "serde", serde(skip))]
    pub test_fd: Option<OwnedFd>,
}

impl SplitFd for LiveUpgradeState {
    fn split_fd(&mut self) -> ApiResult<Fds> {
        let fd = self
            .test_fd
            .take()
            .ok_or_else(|| ApiError::msg("test_fd is not bound"))?;
        Ok(vec![fd])
    }
}

impl MergeFd for LiveUpgradeState {
    fn merge_fd(&mut self, fds: &mut [Option<OwnedFd>]) -> ApiResult<()> {
        if self.test_fd.is_some() {
            return Err(ApiError::msg("test_fd is already bound"));
        }
        if fds.len() != 1 {
            return Err(ApiError::msg(format!(
                "expected 1 fd for LiveUpgradeState, got {}",
                fds.len()
            )));
        }
        let fd = fds[0]
            .take()
            .ok_or_else(|| ApiError::msg("fd at index 0 was already consumed"))?;
        self.test_fd = Some(fd);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn split_and_merge_move_exactly_one_descriptor() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let raw_fd = socket.as_raw_fd();
        let mut state = LiveUpgradeState {
            test_fd: Some(socket.into()),
        };
        let fds = state.split_fd().unwrap();
        assert_eq!(fds.len(), 1);
        assert_eq!(fds[0].as_raw_fd(), raw_fd);
        assert!(state.test_fd.is_none());
        assert_eq!(
            state.split_fd().unwrap_err(),
            ApiError::msg("test_fd is not bound")
        );

        let mut slots: Vec<_> = fds.into_iter().map(Some).collect();
        state.merge_fd(&mut slots).unwrap();
        assert_eq!(slots.len(), 1);
        assert!(slots[0].is_none());
        assert_eq!(state.test_fd.as_ref().unwrap().as_raw_fd(), raw_fd);
        drop(state);
        peer.set_nonblocking(true).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn merge_rejects_invalid_count_and_consumed_slots() {
        let mut state = LiveUpgradeState { test_fd: None };
        assert_eq!(
            state.merge_fd(&mut []).unwrap_err(),
            ApiError::msg("expected 1 fd for LiveUpgradeState, got 0")
        );
        let (socket, _peer) = UnixStream::pair().unwrap();
        let raw_fd = socket.as_raw_fd();
        let mut slots = [Some(socket.into()), None];
        assert_eq!(
            state.merge_fd(&mut slots).unwrap_err(),
            ApiError::msg("expected 1 fd for LiveUpgradeState, got 2")
        );
        assert_eq!(slots[0].as_ref().unwrap().as_raw_fd(), raw_fd);
        assert_eq!(
            state.merge_fd(&mut [None]).unwrap_err(),
            ApiError::msg("fd at index 0 was already consumed")
        );
        assert!(state.test_fd.is_none());
    }

    #[test]
    fn merge_rejects_rebinding_without_consuming_or_replacing_fds() {
        let (first, _first_peer) = UnixStream::pair().unwrap();
        let (second, _second_peer) = UnixStream::pair().unwrap();
        let first_fd = first.as_raw_fd();
        let second_fd = second.as_raw_fd();
        let mut state = LiveUpgradeState {
            test_fd: Some(first.into()),
        };
        let mut slots = [Some(second.into())];
        assert_eq!(
            state.merge_fd(&mut slots).unwrap_err(),
            ApiError::msg("test_fd is already bound")
        );
        assert_eq!(state.test_fd.as_ref().unwrap().as_raw_fd(), first_fd);
        assert_eq!(slots[0].as_ref().unwrap().as_raw_fd(), second_fd);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_roundtrip_keeps_the_fd_out_of_json() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let mut state = LiveUpgradeState {
            test_fd: Some(socket.into()),
        };
        assert_eq!(serde_json::to_value(&state).unwrap(), serde_json::json!({}));
        assert!(state.test_fd.is_some());
        let mut slots: Vec<_> = state.split_fd().unwrap().into_iter().map(Some).collect();
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value, serde_json::json!({}));
        let mut decoded: LiveUpgradeState = serde_json::from_value(value).unwrap();
        assert!(decoded.test_fd.is_none());
        decoded.merge_fd(&mut slots).unwrap();
        assert!(slots[0].is_none());
        assert!(decoded.test_fd.is_some());
    }

    #[cfg(feature = "schema")]
    #[test]
    fn schema_does_not_expose_fd_fields() {
        let schema = schemars::schema_for!(LiveUpgradeState).to_value();
        assert!(schema["properties"].get("test_fd").is_none());
        assert!(schema["properties"].get("test_fd_index").is_none());
    }
}
