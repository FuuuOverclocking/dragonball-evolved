use crate::space::{Gpa, Range};

pub const DEVICE_ID: u32 = 24;
pub const VERSION_1: u64 = 1 << 32;
pub const UNPLUGGED_INACCESSIBLE: u64 = 1 << 1;
pub const REQUIRED_FEATURES: u64 = VERSION_1 | UNPLUGGED_INACCESSIBLE;
pub const OFFERED_FEATURES: u64 = REQUIRED_FEATURES;
pub const QUEUE_INDEX: u16 = 0;
pub const QUEUE_MAX: u16 = 256;

pub const REQUEST_SIZE: usize = 24;
pub const REQUEST_TYPE_OFFSET: usize = 0;
pub const REQUEST_ADDR_OFFSET: usize = 8;
pub const REQUEST_BLOCKS_OFFSET: usize = 16;

pub const RESPONSE_SIZE: usize = 10;
pub const RESPONSE_TYPE_OFFSET: usize = 0;
pub const RESPONSE_STATE_OFFSET: usize = 8;

pub const CONFIG_SIZE: usize = 56;
pub const CONFIG_BLOCK_SIZE_OFFSET: usize = 0;
pub const CONFIG_NODE_ID_OFFSET: usize = 8;
pub const CONFIG_ADDR_OFFSET: usize = 16;
pub const CONFIG_REGION_SIZE_OFFSET: usize = 24;
pub const CONFIG_USABLE_REGION_SIZE_OFFSET: usize = 32;
pub const CONFIG_PLUGGED_SIZE_OFFSET: usize = 40;
pub const CONFIG_REQUESTED_SIZE_OFFSET: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum FeatureError {
    /// required virtio-mem features were not negotiated: {0:#x}
    Missing(u64),
    /// unoffered virtio-mem features were negotiated: {0:#x}
    Unsupported(u64),
}

pub fn validate_features(driver_features: u64) -> Result<(), FeatureError> {
    let missing = REQUIRED_FEATURES & !driver_features;
    if missing != 0 {
        return Err(FeatureError::Missing(missing));
    }
    let unsupported = driver_features & !OFFERED_FEATURES;
    if unsupported != 0 {
        return Err(FeatureError::Unsupported(unsupported));
    }
    Ok(())
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestType {
    Plug = 0,
    Unplug = 1,
    UnplugAll = 2,
    State = 3,
}

impl TryFrom<u16> for RequestType {
    type Error = WireError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Plug),
            1 => Ok(Self::Unplug),
            2 => Ok(Self::UnplugAll),
            3 => Ok(Self::State),
            _ => Err(WireError::UnknownRequestType(value)),
        }
    }
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseType {
    Ack = 0,
    Nack = 1,
    Busy = 2,
    Error = 3,
}

impl TryFrom<u16> for ResponseType {
    type Error = WireError;

    fn try_from(value: u16) -> Result<Self, WireError> {
        match value {
            0 => Ok(Self::Ack),
            1 => Ok(Self::Nack),
            2 => Ok(Self::Busy),
            3 => Ok(Self::Error),
            _ => Err(WireError::UnknownResponseType(value)),
        }
    }
}

impl ResponseType {
    fn valid_for(self, request_type: RequestType) -> bool {
        use RequestType::{Plug, State, Unplug, UnplugAll};
        use ResponseType::{Ack, Busy, Error, Nack};
        matches!(
            (request_type, self),
            (_, Ack)
                | (Plug, Nack)
                | (Plug | Unplug | UnplugAll, Busy)
                | (Plug | Unplug | State, Error)
        )
    }
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    Plugged = 0,
    Unplugged = 1,
    Mixed = 2,
}

impl TryFrom<u16> for BlockState {
    type Error = WireError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Plugged),
            1 => Ok(Self::Unplugged),
            2 => Ok(Self::Mixed),
            _ => Err(WireError::UnknownBlockState(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum WireError {
    /// virtio-mem wire length is {actual}, expected {expected}
    Length { expected: usize, actual: usize },
    /// unknown virtio-mem request type {0}
    UnknownRequestType(u16),
    /// unknown virtio-mem response type {0}
    UnknownResponseType(u16),
    /// unknown virtio-mem block state {0}
    UnknownBlockState(u16),
    /// virtio-mem response {response_type:?} is invalid for {request_type:?}
    ResponseForRequest {
        request_type: RequestType,
        response_type: ResponseType,
    },
    /// invalid virtio-mem configuration
    Config(#[from] ConfigError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Plug { addr: Gpa, blocks: u16 },
    Unplug { addr: Gpa, blocks: u16 },
    UnplugAll,
    State { addr: Gpa, blocks: u16 },
}

impl Request {
    pub const fn request_type(self) -> RequestType {
        match self {
            Self::Plug { .. } => RequestType::Plug,
            Self::Unplug { .. } => RequestType::Unplug,
            Self::UnplugAll => RequestType::UnplugAll,
            Self::State { .. } => RequestType::State,
        }
    }

    fn range_fields(self) -> Option<(Gpa, u16)> {
        match self {
            Self::Plug { addr, blocks }
            | Self::Unplug { addr, blocks }
            | Self::State { addr, blocks } => Some((addr, blocks)),
            Self::UnplugAll => None,
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_length(bytes, REQUEST_SIZE)?;
        let request_type = RequestType::try_from(read_u16(bytes, REQUEST_TYPE_OFFSET))?;
        if request_type == RequestType::UnplugAll {
            return Ok(Self::UnplugAll);
        }
        let addr = Gpa(read_u64(bytes, REQUEST_ADDR_OFFSET));
        let blocks = read_u16(bytes, REQUEST_BLOCKS_OFFSET);
        Ok(match request_type {
            RequestType::Plug => Self::Plug { addr, blocks },
            RequestType::Unplug => Self::Unplug { addr, blocks },
            RequestType::UnplugAll => Self::UnplugAll,
            RequestType::State => Self::State { addr, blocks },
        })
    }

    pub fn encode(self) -> [u8; REQUEST_SIZE] {
        let mut bytes = [0; REQUEST_SIZE];
        write_u16(&mut bytes, REQUEST_TYPE_OFFSET, self.request_type() as u16);
        if let Some((addr, blocks)) = self.range_fields() {
            write_u64(&mut bytes, REQUEST_ADDR_OFFSET, addr.0);
            write_u16(&mut bytes, REQUEST_BLOCKS_OFFSET, blocks);
        }
        bytes
    }

    /// Validates geometry only; population and captured-target admission belong to the service.
    pub fn validate(self, config: &Config) -> Result<Option<Range<Gpa>>, RequestError> {
        let Some((addr, blocks)) = self.range_fields() else {
            return Ok(None);
        };
        config.validate()?;
        if blocks == 0 {
            return Err(RequestError::Empty);
        }
        if !addr.0.is_multiple_of(config.block_size) {
            return Err(RequestError::Unaligned {
                addr,
                block_size: config.block_size,
            });
        }
        let len = config
            .block_size
            .checked_mul(u64::from(blocks))
            .ok_or(RequestError::Overflow)?;
        let range = Range::new(addr, len).ok_or(RequestError::Overflow)?;
        let usable = Range::new(config.addr, config.usable_region_size)
            .ok_or(ConfigError::RegionOverflow)?;
        if !usable.contains_range(range) {
            return Err(RequestError::OutsideUsable(range));
        }
        Ok(Some(range))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum RequestError {
    /// virtio-mem block count must be greater than zero
    Empty,
    /// virtio-mem request address {addr} is not aligned to block size {block_size}
    Unaligned { addr: Gpa, block_size: u64 },
    /// virtio-mem request size or guest-physical range overflows
    Overflow,
    /// virtio-mem request range {0} is outside the usable region
    OutsideUsable(Range<Gpa>),
    /// invalid virtio-mem configuration
    Config(#[from] ConfigError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    Ack,
    Nack,
    Busy,
    Error,
    State(BlockState),
}

impl Response {
    pub const fn response_type(self) -> ResponseType {
        match self {
            Self::Ack | Self::State(_) => ResponseType::Ack,
            Self::Nack => ResponseType::Nack,
            Self::Busy => ResponseType::Busy,
            Self::Error => ResponseType::Error,
        }
    }

    pub const fn state(self) -> Option<BlockState> {
        match self {
            Self::State(state) => Some(state),
            _ => None,
        }
    }

    pub fn decode(bytes: &[u8], request_type: RequestType) -> Result<Self, WireError> {
        require_length(bytes, RESPONSE_SIZE)?;
        let response_type = ResponseType::try_from(read_u16(bytes, RESPONSE_TYPE_OFFSET))?;
        if !response_type.valid_for(request_type) {
            return Err(WireError::ResponseForRequest {
                request_type,
                response_type,
            });
        }
        Ok(match response_type {
            ResponseType::Ack if request_type == RequestType::State => Self::State(
                BlockState::try_from(read_u16(bytes, RESPONSE_STATE_OFFSET))?,
            ),
            ResponseType::Ack => Self::Ack,
            ResponseType::Nack => Self::Nack,
            ResponseType::Busy => Self::Busy,
            ResponseType::Error => Self::Error,
        })
    }

    pub fn encode(self) -> [u8; RESPONSE_SIZE] {
        let mut bytes = [0; RESPONSE_SIZE];
        write_u16(
            &mut bytes,
            RESPONSE_TYPE_OFFSET,
            self.response_type() as u16,
        );
        if let Some(state) = self.state() {
            write_u16(&mut bytes, RESPONSE_STATE_OFFSET, state as u16);
        }
        bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display)]
pub enum ConfigField {
    /// addr
    Addr,
    /// region_size
    RegionSize,
    /// usable_region_size
    UsableRegionSize,
    /// plugged_size
    PluggedSize,
    /// requested_size
    RequestedSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum ConfigError {
    /// virtio-mem block size {0} must be a power of two
    BlockSize(u64),
    /// virtio-mem configuration field {field} is not a multiple of block size {block_size}
    Unaligned { field: ConfigField, block_size: u64 },
    /// virtio-mem device region overflows guest-physical address space
    RegionOverflow,
    /// virtio-mem usable size {usable} exceeds region size {region}
    UsableExceedsRegion { usable: u64, region: u64 },
    /// virtio-mem requested size {requested} exceeds usable size {usable}
    RequestedExceedsUsable { requested: u64, usable: u64 },
    /// virtio-mem plugged size {plugged} exceeds usable size {usable}
    PluggedExceedsUsable { plugged: u64, usable: u64 },
    /// virtio-mem chunk size {0} must be a power of two and a multiple of block size
    ChunkSize(u64),
    /// virtio-mem target {requested} exceeds region capacity {region}
    TargetExceedsRegion { requested: u64, region: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub block_size: u64,
    pub node_id: u16,
    pub addr: Gpa,
    pub region_size: u64,
    pub usable_region_size: u64,
    pub plugged_size: u64,
    pub requested_size: u64,
}

impl Config {
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_length(bytes, CONFIG_SIZE)?;
        let config = Self {
            block_size: read_u64(bytes, CONFIG_BLOCK_SIZE_OFFSET),
            node_id: read_u16(bytes, CONFIG_NODE_ID_OFFSET),
            addr: Gpa(read_u64(bytes, CONFIG_ADDR_OFFSET)),
            region_size: read_u64(bytes, CONFIG_REGION_SIZE_OFFSET),
            usable_region_size: read_u64(bytes, CONFIG_USABLE_REGION_SIZE_OFFSET),
            plugged_size: read_u64(bytes, CONFIG_PLUGGED_SIZE_OFFSET),
            requested_size: read_u64(bytes, CONFIG_REQUESTED_SIZE_OFFSET),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn encode(self) -> [u8; CONFIG_SIZE] {
        let mut bytes = [0; CONFIG_SIZE];
        write_u64(&mut bytes, CONFIG_BLOCK_SIZE_OFFSET, self.block_size);
        write_u16(&mut bytes, CONFIG_NODE_ID_OFFSET, self.node_id);
        write_u64(&mut bytes, CONFIG_ADDR_OFFSET, self.addr.0);
        write_u64(&mut bytes, CONFIG_REGION_SIZE_OFFSET, self.region_size);
        write_u64(
            &mut bytes,
            CONFIG_USABLE_REGION_SIZE_OFFSET,
            self.usable_region_size,
        );
        write_u64(&mut bytes, CONFIG_PLUGGED_SIZE_OFFSET, self.plugged_size);
        write_u64(
            &mut bytes,
            CONFIG_REQUESTED_SIZE_OFFSET,
            self.requested_size,
        );
        bytes
    }

    pub fn validate(self) -> Result<(), ConfigError> {
        if !self.block_size.is_power_of_two() {
            return Err(ConfigError::BlockSize(self.block_size));
        }
        for (field, value) in [
            (ConfigField::Addr, self.addr.0),
            (ConfigField::RegionSize, self.region_size),
            (ConfigField::UsableRegionSize, self.usable_region_size),
            (ConfigField::PluggedSize, self.plugged_size),
            (ConfigField::RequestedSize, self.requested_size),
        ] {
            require_multiple(field, value, self.block_size)?;
        }
        Range::new(self.addr, self.region_size).ok_or(ConfigError::RegionOverflow)?;
        if self.usable_region_size > self.region_size {
            return Err(ConfigError::UsableExceedsRegion {
                usable: self.usable_region_size,
                region: self.region_size,
            });
        }
        if self.requested_size > self.usable_region_size {
            return Err(ConfigError::RequestedExceedsUsable {
                requested: self.requested_size,
                usable: self.usable_region_size,
            });
        }
        if self.plugged_size > self.usable_region_size {
            return Err(ConfigError::PluggedExceedsUsable {
                plugged: self.plugged_size,
                usable: self.usable_region_size,
            });
        }
        Ok(())
    }

    pub fn with_requested_size(
        self,
        requested_size: u64,
        chunk_size: u64,
    ) -> Result<Self, ConfigError> {
        self.validate()?;
        self.validate_chunk_size(chunk_size)?;
        if requested_size > self.region_size {
            return Err(ConfigError::TargetExceedsRegion {
                requested: requested_size,
                region: self.region_size,
            });
        }
        require_multiple(ConfigField::RequestedSize, requested_size, self.block_size)?;
        Ok(Self {
            requested_size,
            usable_region_size: self
                .usable_region_size
                .max(self.rounded_usable_size(requested_size, chunk_size)),
            ..self
        })
    }

    /// Project only successful UNPLUG_ALL completion; the service owns the actual population.
    pub fn after_unplug_all(self, chunk_size: u64) -> Result<Self, ConfigError> {
        self.validate()?;
        self.validate_chunk_size(chunk_size)?;
        Ok(Self {
            plugged_size: 0,
            usable_region_size: self.rounded_usable_size(self.requested_size, chunk_size),
            ..self
        })
    }

    fn validate_chunk_size(self, chunk_size: u64) -> Result<(), ConfigError> {
        if !chunk_size.is_power_of_two() || !chunk_size.is_multiple_of(self.block_size) {
            return Err(ConfigError::ChunkSize(chunk_size));
        }
        Ok(())
    }

    fn rounded_usable_size(self, requested_size: u64, chunk_size: u64) -> u64 {
        let remainder = requested_size % chunk_size;
        if remainder == 0 {
            return requested_size;
        }
        // An unrepresentable rounded size is above every representable region cap.
        requested_size
            .checked_add(chunk_size - remainder)
            .unwrap_or(self.region_size)
            .min(self.region_size)
    }
}

fn require_multiple(field: ConfigField, value: u64, block_size: u64) -> Result<(), ConfigError> {
    if !value.is_multiple_of(block_size) {
        return Err(ConfigError::Unaligned { field, block_size });
    }
    Ok(())
}

fn require_length(bytes: &[u8], expected: usize) -> Result<(), WireError> {
    if bytes.len() != expected {
        return Err(WireError::Length {
            expected,
            actual: bytes.len(),
        });
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            block_size: 0x1000,
            node_id: 0,
            addr: Gpa(0x10000),
            region_size: 0x10000,
            usable_region_size: 0x8000,
            plugged_size: 0x3000,
            requested_size: 0x4000,
        }
    }

    fn ranged_requests(addr: Gpa, blocks: u16) -> [Request; 3] {
        [
            Request::Plug { addr, blocks },
            Request::Unplug { addr, blocks },
            Request::State { addr, blocks },
        ]
    }

    #[test]
    fn identity_queue_and_features_match_the_local_contract() {
        assert_eq!(DEVICE_ID, 24);
        assert_eq!(QUEUE_INDEX, 0);
        assert_eq!(QUEUE_MAX, 256);
        assert_eq!(REQUIRED_FEATURES, 0x0000_0001_0000_0002);
        assert_eq!(OFFERED_FEATURES, REQUIRED_FEATURES);
        assert_eq!(validate_features(REQUIRED_FEATURES), Ok(()));
        assert_eq!(
            validate_features(0),
            Err(FeatureError::Missing(REQUIRED_FEATURES))
        );
        assert_eq!(
            validate_features(VERSION_1),
            Err(FeatureError::Missing(UNPLUGGED_INACCESSIBLE))
        );
        assert_eq!(
            validate_features(UNPLUGGED_INACCESSIBLE),
            Err(FeatureError::Missing(VERSION_1))
        );
        for unoffered in [1 << 0, 1 << 2, 1 << 33, 1 << 63] {
            assert_eq!(
                validate_features(REQUIRED_FEATURES | unoffered),
                Err(FeatureError::Unsupported(unoffered))
            );
        }
    }

    #[test]
    fn requests_use_exact_little_endian_bytes_not_rust_layout() {
        let addr = Gpa(0x8877_6655_4433_2211);
        let blocks = 0x1234;
        let cases = [
            (
                Request::Plug { addr, blocks },
                [
                    0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x34,
                    0x12, 0, 0, 0, 0, 0, 0,
                ],
            ),
            (
                Request::Unplug { addr, blocks },
                [
                    1, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x34,
                    0x12, 0, 0, 0, 0, 0, 0,
                ],
            ),
            (
                Request::State { addr, blocks },
                [
                    3, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x34,
                    0x12, 0, 0, 0, 0, 0, 0,
                ],
            ),
        ];
        for (request, bytes) in cases {
            assert_eq!(request.encode(), bytes);
            assert_eq!(Request::decode(&bytes), Ok(request));
            let mut padded = bytes;
            padded[2..8].fill(0xa5);
            padded[18..24].fill(0xff);
            assert_eq!(Request::decode(&padded), Ok(request));
            assert_eq!(Request::decode(&padded).unwrap().encode(), bytes);
        }
    }

    #[test]
    fn unplug_all_ignores_every_byte_except_its_type() {
        let bytes = [
            2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(Request::UnplugAll.encode(), bytes);
        let ignored = [
            2, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        assert_eq!(Request::decode(&ignored), Ok(Request::UnplugAll));
        assert_eq!(Request::UnplugAll.validate(&config()), Ok(None));
    }

    #[test]
    fn responses_use_ten_bytes_and_state_only_on_state_ack() {
        let cases = [
            (
                RequestType::Plug,
                Response::Ack,
                [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                RequestType::Plug,
                Response::Nack,
                [1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                RequestType::UnplugAll,
                Response::Busy,
                [2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                RequestType::State,
                Response::Error,
                [3, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                RequestType::State,
                Response::State(BlockState::Plugged),
                [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                RequestType::State,
                Response::State(BlockState::Unplugged),
                [0, 0, 0, 0, 0, 0, 0, 0, 1, 0],
            ),
            (
                RequestType::State,
                Response::State(BlockState::Mixed),
                [0, 0, 0, 0, 0, 0, 0, 0, 2, 0],
            ),
        ];
        for (request_type, response, bytes) in cases {
            assert_eq!(response.encode(), bytes);
            assert_eq!(Response::decode(&bytes, request_type), Ok(response));
            let mut padded = bytes;
            padded[2..8].fill(0xa5);
            if response.state().is_none() {
                padded[8..10].fill(0xff);
            }
            assert_eq!(Response::decode(&padded, request_type), Ok(response));
            assert_eq!(
                Response::decode(&padded, request_type).unwrap().encode(),
                bytes
            );
        }
    }

    #[test]
    fn response_codes_must_be_applicable_to_the_request() {
        for request_type in [
            RequestType::Plug,
            RequestType::Unplug,
            RequestType::UnplugAll,
            RequestType::State,
        ] {
            for response_type in [
                ResponseType::Ack,
                ResponseType::Nack,
                ResponseType::Busy,
                ResponseType::Error,
            ] {
                let valid = match response_type {
                    ResponseType::Ack => true,
                    ResponseType::Nack => request_type == RequestType::Plug,
                    ResponseType::Busy => request_type != RequestType::State,
                    ResponseType::Error => request_type != RequestType::UnplugAll,
                };
                let bytes = match response_type {
                    ResponseType::Ack => [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    ResponseType::Nack => [1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    ResponseType::Busy => [2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    ResponseType::Error => [3, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                };
                let result = Response::decode(&bytes, request_type);
                if valid {
                    assert!(result.is_ok());
                } else {
                    assert_eq!(
                        result,
                        Err(WireError::ResponseForRequest {
                            request_type,
                            response_type
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn configuration_uses_exact_fifty_six_byte_layout_and_ignores_padding() {
        let config = Config {
            block_size: 0x0020_0000,
            node_id: 0x1234,
            addr: Gpa(0x8877_6655_4420_0000),
            region_size: 0x1000_0000,
            usable_region_size: 0x0800_0000,
            plugged_size: 0x0060_0000,
            requested_size: 0x0040_0000,
        };
        let bytes = [
            0, 0, 0x20, 0, 0, 0, 0, 0, 0x34, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0x20, 0x44, 0x55, 0x66,
            0x77, 0x88, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0x08, 0, 0, 0, 0, 0, 0, 0x60, 0, 0, 0,
            0, 0, 0, 0, 0x40, 0, 0, 0, 0, 0,
        ];
        assert_eq!(config.encode(), bytes);
        assert_eq!(Config::decode(&bytes), Ok(config));
        let mut padded = bytes;
        padded[10..16].fill(0xff);
        assert_eq!(Config::decode(&padded), Ok(config));
        assert_eq!(Config::decode(&padded).unwrap().encode(), bytes);
    }

    #[test]
    fn wire_boundaries_reject_truncation_extra_bytes_and_unknown_codes() {
        let request = [0; REQUEST_SIZE + 1];
        for actual in (0..REQUEST_SIZE).chain([REQUEST_SIZE + 1]) {
            assert_eq!(
                Request::decode(&request[..actual]),
                Err(WireError::Length {
                    expected: 24,
                    actual
                })
            );
        }
        let response = [0; RESPONSE_SIZE + 1];
        for actual in (0..RESPONSE_SIZE).chain([RESPONSE_SIZE + 1]) {
            assert_eq!(
                Response::decode(&response[..actual], RequestType::Plug),
                Err(WireError::Length {
                    expected: 10,
                    actual
                })
            );
        }
        let config = [0; CONFIG_SIZE + 1];
        for actual in (0..CONFIG_SIZE).chain([CONFIG_SIZE + 1]) {
            assert_eq!(
                Config::decode(&config[..actual]),
                Err(WireError::Length {
                    expected: 56,
                    actual
                })
            );
        }
        let unknown_request = [
            0x34, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(
            Request::decode(&unknown_request),
            Err(WireError::UnknownRequestType(0x1234))
        );
        assert_eq!(
            Response::decode(&[0x34, 0x12, 0, 0, 0, 0, 0, 0, 0, 0], RequestType::Plug),
            Err(WireError::UnknownResponseType(0x1234))
        );
        assert_eq!(
            Response::decode(&[0, 0, 0, 0, 0, 0, 0, 0, 0x34, 0x12], RequestType::State),
            Err(WireError::UnknownBlockState(0x1234))
        );
        assert_eq!(
            Config::decode(&[0; 56]),
            Err(WireError::Config(ConfigError::BlockSize(0)))
        );
    }

    #[test]
    fn configuration_validation_checks_geometry_but_allows_plugged_above_target() {
        let original = config();
        for block_size in [0, 3, 0x3000] {
            assert_eq!(
                Config {
                    block_size,
                    ..original
                }
                .validate(),
                Err(ConfigError::BlockSize(block_size))
            );
        }
        let unaligned = [
            (
                ConfigField::Addr,
                Config {
                    addr: Gpa(0x10001),
                    ..original
                },
            ),
            (
                ConfigField::RegionSize,
                Config {
                    region_size: 0x10001,
                    ..original
                },
            ),
            (
                ConfigField::UsableRegionSize,
                Config {
                    usable_region_size: 0x8001,
                    ..original
                },
            ),
            (
                ConfigField::PluggedSize,
                Config {
                    plugged_size: 0x3001,
                    ..original
                },
            ),
            (
                ConfigField::RequestedSize,
                Config {
                    requested_size: 0x4001,
                    ..original
                },
            ),
        ];
        for (field, invalid) in unaligned {
            let error = ConfigError::Unaligned {
                field,
                block_size: 0x1000,
            };
            assert_eq!(invalid.validate(), Err(error));
            assert_eq!(
                Config::decode(&invalid.encode()),
                Err(WireError::Config(error))
            );
        }
        assert_eq!(
            Config {
                usable_region_size: 0x11000,
                ..original
            }
            .validate(),
            Err(ConfigError::UsableExceedsRegion {
                usable: 0x11000,
                region: 0x10000
            })
        );
        assert_eq!(
            Config {
                requested_size: 0x9000,
                ..original
            }
            .validate(),
            Err(ConfigError::RequestedExceedsUsable {
                requested: 0x9000,
                usable: 0x8000
            })
        );
        assert_eq!(
            Config {
                plugged_size: 0x9000,
                ..original
            }
            .validate(),
            Err(ConfigError::PluggedExceedsUsable {
                plugged: 0x9000,
                usable: 0x8000
            })
        );
        assert_eq!(
            Config {
                requested_size: 0,
                ..original
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            Config {
                region_size: 0,
                usable_region_size: 0,
                plugged_size: 0,
                requested_size: 0,
                ..original
            }
            .validate(),
            Ok(())
        );
    }

    #[test]
    fn plug_unplug_and_state_validate_the_entire_usable_range() {
        let config = config();
        for (addr, blocks) in [(Gpa(0x10000), 8), (Gpa(0x17000), 1)] {
            for request in ranged_requests(addr, blocks) {
                assert_eq!(
                    request.validate(&config),
                    Ok(Some(Range::new(addr, u64::from(blocks) * 0x1000).unwrap()))
                );
            }
        }
        for (addr, blocks) in [
            (Gpa(0xf000), 1),
            (Gpa(0xf000), 2),
            (Gpa(0x17000), 2),
            (Gpa(0x18000), 1),
            (Gpa(0x20000), 1),
        ] {
            for request in ranged_requests(addr, blocks) {
                let range = Range::new(addr, u64::from(blocks) * 0x1000).unwrap();
                assert_eq!(
                    request.validate(&config),
                    Err(RequestError::OutsideUsable(range))
                );
            }
        }
        for request in ranged_requests(config.addr, 0) {
            assert_eq!(request.validate(&config), Err(RequestError::Empty));
        }
        for request in ranged_requests(Gpa(0x10001), 1) {
            assert_eq!(
                request.validate(&config),
                Err(RequestError::Unaligned {
                    addr: Gpa(0x10001),
                    block_size: 0x1000
                })
            );
        }
        let empty = Config {
            usable_region_size: 0,
            plugged_size: 0,
            requested_size: 0,
            ..config
        };
        for request in ranged_requests(empty.addr, 1) {
            assert!(matches!(
                request.validate(&empty),
                Err(RequestError::OutsideUsable(_))
            ));
        }
    }

    #[test]
    fn range_validation_checks_multiplication_and_address_overflow() {
        let config = config();
        for request in ranged_requests(Gpa(u64::MAX - 0xfff), 2) {
            assert_eq!(request.validate(&config), Err(RequestError::Overflow));
        }
        let huge = Config {
            block_size: 1 << 63,
            addr: Gpa(0),
            region_size: 1 << 63,
            usable_region_size: 1 << 63,
            plugged_size: 0,
            requested_size: 0,
            node_id: 0,
        };
        for request in ranged_requests(huge.addr, 2) {
            assert_eq!(request.validate(&huge), Err(RequestError::Overflow));
        }
        let invalid = Config {
            addr: Gpa(u64::MAX - 0xfff),
            region_size: 0x2000,
            usable_region_size: 0,
            plugged_size: 0,
            requested_size: 0,
            ..config
        };
        assert_eq!(invalid.validate(), Err(ConfigError::RegionOverflow));
        assert_eq!(
            Request::State {
                addr: invalid.addr,
                blocks: 1
            }
            .validate(&invalid),
            Err(RequestError::Config(ConfigError::RegionOverflow))
        );
        let top = Config {
            region_size: 0x1000,
            usable_region_size: 0x1000,
            ..invalid
        };
        assert_eq!(top.validate(), Ok(()));
        for request in ranged_requests(top.addr, 1) {
            let range = request.validate(&top).unwrap().unwrap();
            assert_eq!(range.last(), Some(Gpa(u64::MAX)));
            assert_eq!(range.end(), None);
        }
        let max_blocks = Config {
            region_size: u64::from(u16::MAX) * 0x1000,
            usable_region_size: u64::from(u16::MAX) * 0x1000,
            ..config
        };
        for request in ranged_requests(max_blocks.addr, u16::MAX) {
            assert_eq!(
                request.validate(&max_blocks).unwrap().unwrap().len(),
                max_blocks.usable_region_size
            );
        }
    }

    #[test]
    fn shrinking_target_preserves_usable_and_plugged_sizes_even_at_zero() {
        let original = config();
        let grown = original.with_requested_size(0x9000, 0x4000).unwrap();
        assert_eq!(
            grown,
            Config {
                requested_size: 0x9000,
                usable_region_size: 0xc000,
                ..original
            }
        );
        let shrunk = grown.with_requested_size(0x1000, 0x4000).unwrap();
        assert_eq!(
            shrunk,
            Config {
                requested_size: 0x1000,
                ..grown
            }
        );
        assert!(shrunk.plugged_size > shrunk.requested_size);
        let zero = shrunk.with_requested_size(0, 0x4000).unwrap();
        assert_eq!(
            zero,
            Config {
                requested_size: 0,
                ..shrunk
            }
        );
        assert_eq!(zero.validate(), Ok(()));
        assert_eq!(original, config());
    }

    #[test]
    fn target_growth_is_chunk_rounded_and_capped_by_region() {
        let original = Config {
            region_size: 0xe000,
            ..config()
        };
        let grown = original.with_requested_size(0xd000, 0x4000).unwrap();
        assert_eq!(grown.usable_region_size, 0xe000);
        assert_eq!(grown.requested_size, 0xd000);
        assert_eq!(grown.validate(), Ok(()));
        let overflow_rounding = Config {
            block_size: 1,
            addr: Gpa(0),
            region_size: u64::MAX,
            usable_region_size: 0,
            plugged_size: 0,
            requested_size: 0,
            node_id: 0,
        };
        let grown = overflow_rounding
            .with_requested_size(u64::MAX, 1 << 63)
            .unwrap();
        assert_eq!(grown.usable_region_size, u64::MAX);
        assert_eq!(grown.validate(), Ok(()));
    }

    #[test]
    fn invalid_targets_and_chunk_geometry_do_not_produce_new_configurations() {
        let original = config();
        assert_eq!(
            original.with_requested_size(0x11000, 0x4000),
            Err(ConfigError::TargetExceedsRegion {
                requested: 0x11000,
                region: 0x10000
            })
        );
        assert_eq!(
            original.with_requested_size(1, 0x4000),
            Err(ConfigError::Unaligned {
                field: ConfigField::RequestedSize,
                block_size: 0x1000
            })
        );
        for chunk_size in [0, 0x800, 0x3000] {
            assert_eq!(
                original.with_requested_size(0, chunk_size),
                Err(ConfigError::ChunkSize(chunk_size))
            );
            assert_eq!(
                original.after_unplug_all(chunk_size),
                Err(ConfigError::ChunkSize(chunk_size))
            );
        }
        assert_eq!(original, config());
    }

    #[test]
    fn successful_unplug_all_is_the_usable_shrink_exception() {
        let original = config().with_requested_size(0x1000, 0x4000).unwrap();
        assert_eq!(original.usable_region_size, 0x8000);
        let completed = original.after_unplug_all(0x4000).unwrap();
        assert_eq!(
            completed,
            Config {
                plugged_size: 0,
                usable_region_size: 0x4000,
                ..original
            }
        );
        assert_eq!(completed.validate(), Ok(()));
        assert_eq!(original.plugged_size, 0x3000);
        let zero = original.with_requested_size(0, 0x4000).unwrap();
        assert_eq!(zero.usable_region_size, 0x8000);
        assert_eq!(
            zero.after_unplug_all(0x4000).unwrap(),
            Config {
                plugged_size: 0,
                usable_region_size: 0,
                ..zero
            }
        );
    }
}
