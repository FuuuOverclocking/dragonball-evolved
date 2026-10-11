use std::os::fd::OwnedFd;

use crate::ApiResult;

pub type Fds = Vec<OwnedFd>;

pub trait SplitFd {
    /// Moves descriptors out before serialization; on failure callers must discard any partially split state.
    fn split_fd(&mut self) -> ApiResult<Fds> {
        Ok(Fds::new())
    }
}

pub trait MergeFd {
    /// Takes indexed slots without shifting them; callers must reject remaining Some entries and discard partial results on error.
    fn merge_fd(&mut self, _fds: &mut [Option<OwnedFd>]) -> ApiResult<()> {
        Ok(())
    }
}

impl SplitFd for () {}
impl MergeFd for () {}

impl<T: SplitFd, E> SplitFd for Result<T, E> {
    fn split_fd(&mut self) -> ApiResult<Fds> {
        match self {
            Ok(value) => value.split_fd(),
            Err(_) => Ok(Fds::new()),
        }
    }
}

impl<T: MergeFd, E> MergeFd for Result<T, E> {
    fn merge_fd(&mut self, fds: &mut [Option<OwnedFd>]) -> ApiResult<()> {
        match self {
            Ok(value) => value.merge_fd(fds),
            Err(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::AsRawFd;

    use super::*;
    use crate::ApiError;

    #[derive(Default)]
    struct DeviceState {
        indices: Vec<usize>,
        fds: Fds,
    }

    impl SplitFd for DeviceState {
        fn split_fd(&mut self) -> ApiResult<Fds> {
            self.indices = (0..self.fds.len()).collect();
            Ok(std::mem::take(&mut self.fds))
        }
    }

    impl MergeFd for DeviceState {
        fn merge_fd(&mut self, slots: &mut [Option<OwnedFd>]) -> ApiResult<()> {
            for &index in &self.indices {
                let fd = slots
                    .get_mut(index)
                    .and_then(Option::take)
                    .ok_or_else(|| ApiError::msg("invalid or consumed FD index"))?;
                self.fds.push(fd);
            }
            Ok(())
        }
    }

    #[test]
    fn result_preserves_split_errors_and_skips_operation_errors() {
        struct Failing;

        impl SplitFd for Failing {
            fn split_fd(&mut self) -> ApiResult<Fds> {
                Err(ApiError::msg("split failed"))
            }
        }

        let mut value: ApiResult<Failing> = Ok(Failing);
        assert_eq!(value.split_fd().unwrap_err(), ApiError::msg("split failed"));
        let mut error: ApiResult<Failing> = Err(ApiError::msg("operation failed"));
        assert!(error.split_fd().unwrap().is_empty());
        assert_eq!(error.err().unwrap(), ApiError::msg("operation failed"));
        assert!(().split_fd().unwrap().is_empty());
    }

    #[test]
    fn variable_length_lists_can_be_partitioned_without_shifting_indices() {
        for count in [0, 1, 17, 64] {
            let mut source = DeviceState {
                fds: (0..count)
                    .map(|_| File::open("/dev/null").unwrap().into())
                    .collect(),
                ..Default::default()
            };
            let raw_fds: Vec<_> = source.fds.iter().map(AsRawFd::as_raw_fd).collect();
            let mut slots: Vec<_> = source.split_fd().unwrap().into_iter().map(Some).collect();
            assert_eq!(source.indices, (0..count).collect::<Vec<_>>());
            assert!(source.fds.is_empty());

            let mut even = DeviceState {
                indices: (0..count).step_by(2).collect(),
                ..Default::default()
            };
            let mut odd = DeviceState {
                indices: (1..count).step_by(2).rev().collect(),
                ..Default::default()
            };
            odd.merge_fd(&mut slots).unwrap();
            even.merge_fd(&mut slots).unwrap();
            for device in [&even, &odd] {
                for (&index, fd) in device.indices.iter().zip(&device.fds) {
                    assert_eq!(fd.as_raw_fd(), raw_fds[index]);
                }
            }
            assert!(slots.iter().all(Option::is_none));
        }
    }
}
