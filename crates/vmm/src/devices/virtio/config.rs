use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSnapshot {
    pub bytes: Vec<u8>,
    pub generation: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum ConfigError {
    /// virtio configuration length {actual} does not match {expected}
    Length { expected: usize, actual: usize },
    /// virtio configuration image does not match the expected bytes
    Image,
    /// virtio configuration reader does not match the saved state
    Snapshot,
}

#[derive(Debug)]
pub struct ConfigPublisher {
    state: Arc<RwLock<ConfigSnapshot>>,
}

#[derive(Debug, Clone)]
pub struct ConfigReader {
    source: ConfigSource,
}

#[derive(Debug, Clone)]
enum ConfigSource {
    Fixed(Arc<[u8]>),
    Dynamic(Arc<RwLock<ConfigSnapshot>>),
}

impl ConfigPublisher {
    pub fn new(bytes: Vec<u8>) -> (Self, ConfigReader) {
        Self::from_snapshot(ConfigSnapshot {
            bytes,
            generation: 0,
        })
    }

    pub fn restore(
        snapshot: ConfigSnapshot,
        expected_bytes: &[u8],
    ) -> Result<(Self, ConfigReader), ConfigError> {
        check_length(snapshot.bytes.len(), expected_bytes.len())?;
        if snapshot.bytes != expected_bytes {
            return Err(ConfigError::Image);
        }
        Ok(Self::from_snapshot(snapshot))
    }

    fn from_snapshot(snapshot: ConfigSnapshot) -> (Self, ConfigReader) {
        let state = Arc::new(RwLock::new(snapshot));
        let reader = ConfigReader {
            source: ConfigSource::Dynamic(Arc::clone(&state)),
        };
        (Self { state }, reader)
    }

    pub fn publish(&mut self, bytes: &[u8]) -> Result<bool, ConfigError> {
        let mut state = self.state.write().unwrap();
        check_length(bytes.len(), state.bytes.len())?;
        if state.bytes == bytes {
            return Ok(false);
        }
        state.bytes.copy_from_slice(bytes);
        state.generation = state.generation.wrapping_add(1);
        Ok(true)
    }
}

impl ConfigReader {
    pub fn fixed(bytes: Vec<u8>) -> Self {
        Self {
            source: ConfigSource::Fixed(bytes.into()),
        }
    }

    pub fn read(&self, offset: usize, data: &mut [u8]) {
        data.fill(0);
        self.with_image(|bytes, _| {
            if let Some(end) = offset.checked_add(data.len())
                && let Some(bytes) = bytes.get(offset..end)
            {
                data.copy_from_slice(bytes);
            }
        });
    }

    pub fn snapshot(&self) -> ConfigSnapshot {
        self.with_image(|bytes, generation| ConfigSnapshot {
            bytes: bytes.to_vec(),
            generation,
        })
    }

    pub fn generation(&self) -> u32 {
        self.with_image(|_, generation| generation)
    }

    pub(crate) fn validate(&self, baseline: &[u8]) -> Result<(), ConfigError> {
        self.with_image(|bytes, _| {
            check_length(bytes.len(), baseline.len())?;
            if matches!(&self.source, ConfigSource::Fixed(_)) && bytes != baseline {
                return Err(ConfigError::Image);
            }
            Ok(())
        })
    }

    pub(crate) fn validate_restore(
        &self,
        baseline: &[u8],
        saved: &ConfigSnapshot,
    ) -> Result<(), ConfigError> {
        self.validate(baseline)?;
        check_length(saved.bytes.len(), baseline.len())?;
        self.with_image(|bytes, generation| {
            if bytes != saved.bytes || generation != saved.generation {
                return Err(ConfigError::Snapshot);
            }
            Ok(())
        })
    }

    fn with_image<T>(&self, read: impl FnOnce(&[u8], u32) -> T) -> T {
        match &self.source {
            ConfigSource::Fixed(bytes) => read(bytes, 0),
            ConfigSource::Dynamic(state) => {
                let state = state.read().unwrap();
                read(&state.bytes, state.generation)
            }
        }
    }
}

fn check_length(actual: usize, expected: usize) -> Result<(), ConfigError> {
    if actual != expected {
        return Err(ConfigError::Length { expected, actual });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_changes_full_images_only_and_preserves_length() {
        let (mut publisher, reader) = ConfigPublisher::new(vec![1; 8]);
        let other = reader.clone();
        assert_eq!(reader.generation(), 0);
        assert_eq!(publisher.publish(&[1; 8]), Ok(false));
        assert_eq!(publisher.publish(&[2; 8]), Ok(true));
        let changed = ConfigSnapshot {
            bytes: vec![2; 8],
            generation: 1,
        };
        assert_eq!(other.snapshot(), changed);
        assert_eq!(publisher.publish(&[2; 8]), Ok(false));
        assert_eq!(reader.snapshot(), changed);
        assert_eq!(
            publisher.publish(&[3; 7]),
            Err(ConfigError::Length {
                expected: 8,
                actual: 7,
            })
        );
        assert_eq!(reader.snapshot(), changed);
        let mut bytes = [0; 4];
        reader.read(2, &mut bytes);
        assert_eq!(bytes, [2; 4]);
        reader.read(usize::MAX, &mut bytes);
        assert_eq!(bytes, [0; 4]);
    }

    #[test]
    fn snapshots_keep_bytes_and_generation_coherent_during_publication() {
        let (mut publisher, reader) = ConfigPublisher::new(vec![0; 64]);
        std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                for generation in 1u32..=1024 {
                    publisher.publish(&[generation as u8; 64]).unwrap();
                }
            });
            for _ in 0..1024 {
                let snapshot = reader.snapshot();
                assert_eq!(snapshot.bytes, vec![snapshot.generation as u8; 64]);
                let mut bytes = [0; 64];
                reader.read(0, &mut bytes);
                assert!(bytes.iter().all(|byte| *byte == bytes[0]));
            }
            writer.join().unwrap();
        });
        assert_eq!(reader.generation(), 1024);
    }

    #[test]
    fn restore_requires_owner_validated_image_and_generation_wraps_on_change() {
        let saved = ConfigSnapshot {
            bytes: vec![1; 4],
            generation: u32::MAX,
        };
        assert!(matches!(
            ConfigPublisher::restore(saved.clone(), &[2; 4]),
            Err(ConfigError::Image)
        ));
        assert!(matches!(
            ConfigPublisher::restore(saved.clone(), &[1; 3]),
            Err(ConfigError::Length { .. })
        ));
        let (mut publisher, reader) = ConfigPublisher::restore(saved.clone(), &[1; 4]).unwrap();
        assert_eq!(reader.snapshot(), saved);
        assert_eq!(publisher.publish(&[1; 4]), Ok(false));
        assert_eq!(reader.generation(), u32::MAX);
        assert_eq!(publisher.publish(&[2; 4]), Ok(true));
        assert_eq!(reader.generation(), 0);
    }

    #[test]
    fn fixed_readers_have_no_writer_and_accept_only_baseline_zero_generation() {
        let reader = ConfigReader::fixed(vec![1; 4]);
        let baseline = [1; 4];
        let saved = reader.snapshot();
        assert_eq!(saved.generation, 0);
        assert_eq!(reader.clone().snapshot(), saved);
        assert_eq!(reader.validate_restore(&baseline, &saved), Ok(()));
        let mut bad = saved.clone();
        bad.generation = 1;
        assert_eq!(
            reader.validate_restore(&baseline, &bad),
            Err(ConfigError::Snapshot)
        );
        bad = saved;
        bad.bytes[0] = 2;
        assert_eq!(
            reader.validate_restore(&baseline, &bad),
            Err(ConfigError::Snapshot)
        );
        assert_eq!(reader.validate(&[2; 4]), Err(ConfigError::Image));
    }
}
