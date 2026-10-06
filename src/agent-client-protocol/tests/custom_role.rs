use std::future::Future;

use agent_client_protocol::{ConnectionTo, Dispatch, Error, Handled, Role, RoleId, UntypedRole};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct NonDefaultRole(u8);

impl Role for NonDefaultRole {
    type Counterpart = Self;

    fn role_id(&self) -> RoleId {
        RoleId::from_singleton(self)
    }

    fn default_handle_dispatch_from(
        &self,
        message: Dispatch,
        _connection: ConnectionTo<Self>,
    ) -> impl Future<Output = Result<Handled<Dispatch>, Error>> + Send {
        std::future::ready(Ok(Handled::No {
            message,
            retry: false,
        }))
    }

    fn counterpart(&self) -> Self::Counterpart {
        self.clone()
    }
}

#[test]
fn singleton_role_id_does_not_require_default() {
    let first = NonDefaultRole(1);
    let second = NonDefaultRole(2);

    assert_eq!(first.role_id(), RoleId::from_singleton(&first));
    assert_eq!(first.role_id(), second.role_id());
    assert_ne!(first.role_id(), UntypedRole.role_id());
}
