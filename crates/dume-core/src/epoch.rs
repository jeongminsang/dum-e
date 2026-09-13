use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EpochError {
    #[error("Stale epoch: attempt was started under epoch {attempt_epoch}, but coordinator is now at epoch {current_epoch}")]
    StaleEpoch {
        attempt_epoch: i64,
        current_epoch: i64,
    },
    #[error("Lease expired: attempt lease expired at {lease_expires_at}, current time is {now}")]
    LeaseExpired {
        lease_expires_at: i64,
        now: i64,
    },
    #[error("Invalid epoch transition: current {current}, attempted {attempted}")]
    InvalidTransition {
        current: i64,
        attempted: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Epoch(pub i64);

impl Epoch {
    pub fn new(val: i64) -> Self {
        Self(val)
    }

    pub fn next(&self) -> Self {
        Self(self.0 + 1)
    }

    pub fn validate_attempt(&self, attempt_epoch: i64) -> Result<(), EpochError> {
        if attempt_epoch != self.0 {
            Err(EpochError::StaleEpoch {
                attempt_epoch,
                current_epoch: self.0,
            })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_epoch_monotonicity_and_fencing() {
        let epoch = Epoch::new(5);
        assert_eq!(epoch.next(), Epoch::new(6));

        assert!(epoch.validate_attempt(5).is_ok());

        let stale = epoch.validate_attempt(4);
        assert_eq!(
            stale,
            Err(EpochError::StaleEpoch {
                attempt_epoch: 4,
                current_epoch: 5,
            })
        );

        let future = epoch.validate_attempt(6);
        assert_eq!(
            future,
            Err(EpochError::StaleEpoch {
                attempt_epoch: 6,
                current_epoch: 5,
            })
        );
    }
}
