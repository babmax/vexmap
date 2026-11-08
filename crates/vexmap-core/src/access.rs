//! Who may do what. Pure functions over roles, so every rule is testable without a database.

use crate::CoreError;
use crate::types::Role;

/// `actor` is `None` for a local mod acting for itself, which is trusted.
pub(crate) fn check_set_role(actor: Option<Role>, target: Role, new: Role) -> Result<(), CoreError> {
    let Some(actor) = actor else { return Ok(()) };
    match actor {
        Role::Owner => Ok(()),
        Role::Admin if target < Role::Admin && new < Role::Admin => Ok(()),
        Role::Admin => Err(CoreError::Forbidden("admins can only manage roles below admin")),
        _ => Err(CoreError::Forbidden("only admins and the owner can manage roles")),
    }
}

pub(crate) fn check_set_feature(actor: Option<Role>) -> Result<(), CoreError> {
    match actor {
        None | Some(Role::Admin | Role::Owner) => Ok(()),
        Some(_) => Err(CoreError::Forbidden("only admins and the owner can switch modules")),
    }
}

/// Rulers join these in Phase 7, when countries exist as objects.
pub(crate) fn check_set_country(actor: Option<Role>) -> Result<(), CoreError> {
    match actor {
        None | Some(Role::Admin | Role::Owner) => Ok(()),
        Some(_) => Err(CoreError::Forbidden("only admins and the owner can change a player's country")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 5] = [Role::Player, Role::Trusted, Role::Moderator, Role::Admin, Role::Owner];

    #[test]
    fn local_mods_and_the_owner_may_set_any_role() {
        for target in ROLES {
            for new in ROLES {
                assert!(check_set_role(None, target, new).is_ok());
                assert!(check_set_role(Some(Role::Owner), target, new).is_ok());
            }
        }
    }

    #[test]
    fn admins_stay_strictly_below_their_own_rank() {
        for target in ROLES {
            for new in ROLES {
                let allowed = check_set_role(Some(Role::Admin), target, new).is_ok();
                assert_eq!(allowed, target < Role::Admin && new < Role::Admin, "{target:?} -> {new:?}");
            }
        }
    }

    #[test]
    fn nobody_below_admin_manages_roles_or_modules() {
        for actor in [Role::Player, Role::Trusted, Role::Moderator] {
            assert!(check_set_role(Some(actor), Role::Player, Role::Player).is_err());
            assert!(check_set_feature(Some(actor)).is_err());
            assert!(check_set_country(Some(actor)).is_err());
        }
        assert!(check_set_feature(Some(Role::Admin)).is_ok());
        assert!(check_set_country(Some(Role::Admin)).is_ok());
        assert!(check_set_country(None).is_ok());
    }
}
