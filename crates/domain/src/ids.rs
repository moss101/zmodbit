//! Opaque 128-bit identity types (docs/13 § Identity types). IDs are UUIDv7
//! (ULID-style, time-ordered) and are never reused; user-visible names are
//! never identifiers.

use std::fmt;

use uuid::Uuid;

macro_rules! define_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a fresh time-ordered identifier.
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID (e.g. loaded from storage).
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }

            pub fn parse(s: &str) -> Result<Self, uuid::Error> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_id!(
    /// Durable interaction container identity.
    SessionId
);
define_id!(
    /// User goal identity.
    TaskId
);
define_id!(
    /// One execution attempt of a task.
    RunId
);
define_id!(
    /// One model interaction cycle.
    TurnId
);
define_id!(
    /// One typed atomic runtime step.
    RunStepId
);
define_id!(TenantId);
define_id!(UserId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_unique_and_ordered() {
        let a = TaskId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = TaskId::generate();
        assert_ne!(a, b);
        assert!(a < b, "uuid v7 must be time-ordered");
    }

    #[test]
    fn ids_round_trip_through_string() {
        let id = SessionId::generate();
        assert_eq!(SessionId::parse(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn ids_are_distinct_types() {
        let t = TaskId::generate();
        // SessionId::from_uuid compiles; the types are not interchangeable
        // except through the explicit constructor, which is the point of
        // opaque identity types.
        let s = SessionId::from_uuid(t.as_uuid());
        assert_eq!(s.as_uuid(), t.as_uuid());
    }
}
