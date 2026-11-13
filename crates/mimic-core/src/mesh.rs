//! The mimic mesh, pure part (spec §7, §7.9; design `docs/design/mimic-mesh.md` §4.2, §6):
//! the plain input structs the controller fills from its watches, the policy of `MimicConfig`s
//! (precedence, validation, defaults) and the scoped view of a deployment.

pub mod head;
pub mod input;
pub mod lb;
pub mod policy;
pub mod view;

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_consent;
#[cfg(test)]
mod tests_from_json;
#[cfg(test)]
mod tests_head;
#[cfg(test)]
mod tests_lb;
#[cfg(test)]
mod tests_policy;
#[cfg(test)]
mod tests_prop;
