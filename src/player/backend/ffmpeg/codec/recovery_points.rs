use super::*;

impl VideoRecoveryPointKind {
    pub(in super::super) fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Cra => "CRA",
            Self::Idr => "IDR",
            Self::Bla => "BLA",
            Self::Keyframe => "KEYFRAME",
        }
    }

    pub(in super::super) fn is_recovery_point(self) -> bool {
        !matches!(self, Self::None)
    }
}
