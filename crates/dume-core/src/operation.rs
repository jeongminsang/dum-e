use crate::types::ExternalOperationStatus;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OperationError {
    #[error("Invalid state transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: ExternalOperationStatus,
        to: ExternalOperationStatus,
    },
    #[error("Operation is in outcome_unknown status and cannot be automatically re-dispatched")]
    OutcomeUnknownGuard,
}

pub fn validate_operation_transition(
    from: ExternalOperationStatus,
    to: ExternalOperationStatus,
) -> Result<(), OperationError> {
    use ExternalOperationStatus::*;

    match (from, to) {
        (IntentRecorded, Dispatched) => Ok(()),
        (IntentRecorded, Failed) => Ok(()),
        (Dispatched, Confirmed) => Ok(()),
        (Dispatched, OutcomeUnknown) => Ok(()),
        (Dispatched, Failed) => Ok(()),
        (OutcomeUnknown, Confirmed) => Ok(()), // Manual or receipt-verified reconciliation
        (OutcomeUnknown, Failed) => Ok(()),
        (OutcomeUnknown, Dispatched) => Err(OperationError::OutcomeUnknownGuard),
        _ => Err(OperationError::InvalidTransition { from, to }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_operation_state_transitions() {
        use ExternalOperationStatus::*;

        assert!(validate_operation_transition(IntentRecorded, Dispatched).is_ok());
        assert!(validate_operation_transition(Dispatched, Confirmed).is_ok());
        assert!(validate_operation_transition(Dispatched, OutcomeUnknown).is_ok());

        // OutcomeUnknown cannot be automatically re-dispatched
        assert_eq!(
            validate_operation_transition(OutcomeUnknown, Dispatched),
            Err(OperationError::OutcomeUnknownGuard)
        );
    }
}
