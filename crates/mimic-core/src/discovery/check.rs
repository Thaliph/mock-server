//! The structural check of a pushed report (spec §7.3): every bound of §6, so that a pod that
//! can push cannot make the controller store more than a sidecar could have recorded. The
//! checks sit next to the types they bound (`Report::check`, `Schema::check`, ...).

/// Why a pushed report is refused. `reason` is the fixed string logged and returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    Malformed,
    UnknownField,
    Version,
    PushLimit,
    Samples,
    Hosts,
    HostLength,
    Status,
    Bucket,
    SchemaNodes,
    SchemaLevels,
    SchemaCut,
    NameLength,
    Params,
    VolatileFields,
    Sketch,
    PathLength,
    PathSegments,
    MediaTypes,
    BodySize,
    BodyMedia,
    SampleBytes,
}

impl Violation {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::UnknownField => "unknown_field",
            Self::Version => "version",
            Self::PushLimit => "push_limit",
            Self::Samples => "samples",
            Self::Hosts => "hosts",
            Self::HostLength => "host_length",
            Self::Status => "status",
            Self::Bucket => "bucket",
            Self::SchemaNodes => "schema_nodes",
            Self::SchemaLevels => "schema_levels",
            Self::SchemaCut => "schema_cut",
            Self::NameLength => "name_length",
            Self::Params => "params",
            Self::VolatileFields => "volatile_fields",
            Self::Sketch => "sketch",
            Self::PathLength => "path_length",
            Self::PathSegments => "path_segments",
            Self::MediaTypes => "media_types",
            Self::BodySize => "body_size",
            Self::BodyMedia => "body_media",
            Self::SampleBytes => "sample_bytes",
        }
    }

    /// `507` instead of `400`.
    pub fn is_push_limit(self) -> bool {
        self == Self::PushLimit
    }
}
