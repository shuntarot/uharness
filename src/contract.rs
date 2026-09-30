//! Checks that each bundle's contract matches the shape of its ports.
//!
//! The contract follows from the set of port roles: `valid` and `ready` give
//! `valid_ready`, `valid` alone gives `valid_only`, neither gives
//! `fixed_latency`, and an AXI4 port gives `axi`. A written contract is checked
//! against this, so a mismatch fails before generation.
//!
//! ## How roles are assigned
//!
//! A suffix dictionary is the default, and the `ports` table overrides it:
//!
//! ```toml
//! ports = { valid = "i_push", ready = "!o_full", data = "i_data" }
//! ```
//!
//! Keep the dictionary conservative. Names such as `full`, `empty` or `busy`
//! are often an inverted ready or valid, and they are not in the dictionary.
//! A handshake with the wrong polarity silently drops beats. Instead, the error
//! names them as candidates and shows how to write `!`.
//!
//! `assign_roles` is the policy (dictionary and overrides); grow the dictionary
//! there. `check_contract` holds what each contract requires.

use std::collections::BTreeMap;

use miette::Diagnostic;
use thiserror::Error;

use crate::bundle::{Binding, DirectionPrefixes};
use crate::dut::{Dut, Port, PortDirection};
use crate::manifest::{Backing, Contract, Manifest, PortSpec, Role};

/// Names that are often an inverted ready or valid. They are not in the
/// dictionary (no inference), but an error names them as candidates.
const INVERSION_HINTS: &[&str] = &["full", "empty", "busy", "stall", "wait", "hold", "nack"];

/// The resolved contract of one bundle.
#[derive(Debug, PartialEq, Eq)]
pub struct BundleContract {
    pub bundle: String,

    /// The resolved contract. If none was written, it is inferred from the
    /// port roles.
    pub contract: Contract,

    /// Whether the contract was written. It is shown in the output, so an
    /// inferred contract does not look like a silent default.
    pub declared: bool,

    /// The role of each port. It always covers every port of the bundle.
    pub roles: Vec<RoleAssignment>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RoleAssignment {
    pub port: String,
    pub role: Role,

    /// Written with `!` (active low, or inverted).
    pub invert: bool,

    pub source: RoleSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleSource {
    /// Named in the `ports` table.
    Explicit,
    /// Found by the suffix dictionary.
    Dictionary,
    /// Found by neither; treated as payload.
    Payload,
}

impl RoleSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoleSource::Explicit => "explicit",
            RoleSource::Dictionary => "dictionary",
            RoleSource::Payload => "payload",
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error, Diagnostic)]
pub enum ContractError {
    /// A written contract is a claim, and it is checked against the ports.
    #[error("[bundle.{bundle}] says `contract = \"{declared}\"`, and its ports say `{inferred}`")]
    #[diagnostic(
        code(harness::contract::contract_disagrees),
        help(
            "The contract follows from the ports, so you do not have to write it. Roles found:\n{roles}\n\n`valid` + `ready` is `valid_ready`, `valid` alone is `valid_only`, and neither is `fixed_latency`.\n\nRemove the line, or fix the wrong one. The usual cause is a port with the wrong role."
        )
    )]
    ContractDisagrees {
        bundle: String,
        declared: &'static str,
        inferred: &'static str,
        roles: String,
    },

    /// A line with no effect is worse than no line, because it reads as if it
    /// had one.
    #[error("[bundle.{bundle}] says `contract = \"{declared}\"`, which it does not get to choose")]
    #[diagnostic(
        code(harness::contract::contract_not_yours),
        help(
            "Here {why}, so the contract is always `{inferred}`. The line has no effect.\n\nRemove it."
        )
    )]
    ContractNotYours {
        bundle: String,
        declared: &'static str,
        inferred: &'static str,
        why: &'static str,
    },

    /// A line with no effect. With a handshake, the DUT says when the data is
    /// there.
    #[error("[bundle.{bundle}] sets `latency`, but its ports make it `{contract}`")]
    #[diagnostic(
        code(harness::contract::latency_not_allowed),
        help(
            "With a handshake, the DUT says when the data is there, so a latency has no effect. Roles found:\n{roles}\n\nRemove `latency`. If the port really has no handshake, fix the role that made it `{contract}`."
        )
    )]
    LatencyNotAllowed {
        bundle: String,
        contract: &'static str,
        roles: String,
    },

    #[error("[bundle.{bundle}] has a `ready` with no `valid`")]
    #[diagnostic(
        code(harness::contract::half_handshake),
        help(
            "A `ready` alone is not a handshake, because nothing says when the data is there. Roles found:\n{roles}\n\nName the `valid` too. Or remove the `ready`, and the bundle becomes `fixed_latency` with a `latency`."
        )
    )]
    HalfHandshake { bundle: String, roles: String },

    #[error("[bundle.{bundle}] mixes an AXI4 interface with other ports")]
    #[diagnostic(
        code(harness::contract::axi4_not_alone),
        help(
            "A bundle with an AXI4 interface holds only that interface. These ports are also in it: {others}.\n\nMove them to their own bundle."
        )
    )]
    Axi4NotAlone { bundle: String, others: String },

    #[error("[bundle.{bundle}] is `reg`, but `{port}` says the DUT drives the address")]
    #[diagnostic(
        code(harness::contract::memory_driven_by_the_dut),
        help(
            "In a `reg` bundle with an address, the host reads the DUT, so the address is a DUT input. Here the DUT drives it, so the DUT is reading a memory. Use `bram` for that:\n\n    [bundle.{bundle}]\n    backing = \"bram\"\n\nIf the host should read the DUT, make the address port a DUT input."
        )
    )]
    MemoryDrivenByTheDut { bundle: String, port: String },

    #[error("[bundle.{bundle}] gives `{port}` the `{role}` role, which `{backing}` has no use for")]
    #[diagnostic(
        code(harness::contract::memory_role_on_non_memory),
        help(
            "`addr` / `rdata` / `wdata` / `we` / `wstrb` / `re` describe an addressable port. The backing depends on who drives the address:\n\n    the DUT drives it   -> `bram` or `bram_preload`\n    the DUT receives it -> `slave`\n\nChange the backing, or use the roles this backing knows: `valid`, `ready`, `data`."
        )
    )]
    MemoryRoleOnNonMemory {
        bundle: String,
        port: String,
        role: &'static str,
        backing: String,
    },

    #[error("[bundle.{bundle}] gives `{port}` the `{role}` role, which `{backing}` has no use for")]
    #[diagnostic(
        code(harness::contract::host_mem_role_on_other),
        help(
            "`rd_*` / `wr_*` describe a transfer-level port (\"read N bytes from A\"). Only `bram` and `host_mem` can serve one.\n\nChange the backing, or use the roles this backing knows."
        )
    )]
    HostMemRoleOnOther {
        bundle: String,
        port: String,
        role: &'static str,
        backing: String,
    },

    #[error("[bundle.{bundle}] has {have} of that half of the `host_mem` port but not `{missing}`")]
    #[diagnostic(
        code(harness::contract::host_mem_incomplete),
        help(
            "Each half of a transfer-level port is stated whole or left out. Reading needs `rd_cmd_valid` / `rd_cmd_ready` / `rd_cmd_addr` / `rd_cmd_size` and `rd_valid` / `rd_ready` / `rd_data` / `rd_last`; writing needs the `wr_` equivalents plus `wr_done_valid`, which is what ends the transfer. A DUT that only reads leaves every `wr_` role out.

Without `{missing}` the terminator cannot tell {why}."
        )
    )]
    HostMemIncomplete {
        bundle: String,
        have: usize,
        missing: &'static str,
        why: &'static str,
    },

    /// A completion carries only "written". Per-tag completion and error
    /// reports need several requests in flight, which is not generated.
    #[error("[bundle.{bundle}] uses `{role}`, which is not generated yet")]
    #[diagnostic(
        code(harness::contract::host_mem_role_unsupported),
        help("`{role}` is not supported yet. Remove it for now.")
    )]
    HostMemRoleUnsupported { bundle: String, role: &'static str },

    /// `valid_only` drops beats, so the user must state it. It must not be
    /// reached by accident.
    #[error("[bundle.{bundle}] has no contract, and its ports do not form a valid_ready handshake")]
    #[diagnostic(
        code(harness::contract::contract_required),
        help(
            "The ports have a `valid` and no `ready`. That is `valid_only`: the DUT cannot be stalled, and beats the harness cannot take are dropped and counted. Roles found:\n{roles}\n{hint}Say which it is:\n\n    ports = {{ .., ready = \"!<port>\" }}   # it was an inverted ready after all\n    contract = \"valid_only\"              # no, the DUT really cannot be stalled"
        )
    )]
    ContractRequired {
        bundle: String,
        roles: String,
        hint: String,
    },

    #[error("[bundle.{bundle}] has two `{role}` ports: {ports}")]
    #[diagnostic(
        code(harness::contract::duplicate_role),
        help(
            "A bundle carries one handshake. Split it into two bundles, or state the roles so that exactly one port is the `{role}`:\n\n    ports = {{ {role} = \"<port>\", .. }}"
        )
    )]
    DuplicateRole {
        bundle: String,
        role: Role,
        ports: String,
    },

    /// Built as is, the terminator would never apply back pressure.
    #[error(
        "[bundle.{bundle}] has `{first_role}` ({first}) and `{second_role}` ({second}) in the same direction ({direction})"
    )]
    #[diagnostic(
        code(harness::contract::same_direction),
        help(
            "`{first_role}` comes from the sender and `{second_role}` from the receiver, so they cannot have the same direction. One of them has the wrong role. Name the roles:\n\n    ports = {{ {first_role} = \"..\", {second_role} = \"..\" }}"
        )
    )]
    SameDirection {
        bundle: String,
        first_role: Role,
        first: String,
        second_role: Role,
        second: String,
        direction: &'static str,
    },
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolves the contract of every bundle.
pub fn resolve(
    dut: &Dut,
    manifest: &Manifest,
    bindings: &[Binding],
    prefixes: &DirectionPrefixes,
) -> Result<Vec<BundleContract>, ContractError> {
    bindings
        .iter()
        .map(|binding| {
            let contract = resolve_one(dut, manifest, binding, prefixes)?;
            // `latency` has meaning only for `fixed_latency`. A written
            // contract is checked in `Manifest::validate`; an inferred one is
            // known only here.
            if manifest.bundle[&binding.bundle].latency.is_some()
                && contract.contract != Contract::FixedLatency
            {
                return Err(ContractError::LatencyNotAllowed {
                    bundle: binding.bundle.clone(),
                    contract: contract.contract.as_str(),
                    roles: role_list(&contract.roles),
                });
            }
            Ok(contract)
        })
        .collect()
}

/// Resolves the contract of one bundle.
fn resolve_one(
    dut: &Dut,
    manifest: &Manifest,
    binding: &Binding,
    prefixes: &DirectionPrefixes,
) -> Result<BundleContract, ContractError> {
    let declared = manifest.bundle[&binding.bundle].contract;
    let spec = manifest.bundle[&binding.bundle].ports.as_ref();
    // The dictionary has memory names only for memory terminators.
    let memory = manifest.bundle[&binding.bundle].backing.is_memory();
    let mut roles = assign_roles(dut, binding, spec, prefixes, memory);
    // AXI4 comes from the type, not from a name or `ports`: a modport of
    // `std::axi4_if` is the role. Without this, it would become payload.
    for assignment in &mut roles {
        if dut
            .ports
            .iter()
            .any(|port| port.name == assignment.port && port.axi4.is_some())
        {
            assignment.role = Role::Axi4;
        }
    }

    // Memory roles on a non-memory backing. The dictionary gives memory roles
    // only for memory backings, so these were written explicitly.
    //
    // A `slave` bundle uses the same names as `bram`, with the directions
    // reversed: `addr` is a DUT input, the host starts each access, and the
    // DUT answers. The direction is a DUT fact, so the user does not declare it.
    let backing = manifest.bundle[&binding.bundle].backing;
    if !memory && let Some(found) = roles.iter().find(|r| r.role.is_memory()) {
        let addr_port = roles
            .iter()
            .find(|r| r.role == Role::Addr)
            .and_then(|r| dut.ports.iter().find(|port| port.name == r.port));
        // A missing `addr` is not refused here; `terminator::resolve_slaves`
        // reports it.
        let slave = backing == Backing::Slave
            && addr_port.is_none_or(|port| port.direction == PortDirection::Input);
        if !slave {
            // `slave` with `addr` as a DUT output means the DUT reads a
            // memory. Suggest `bram` by name.
            return Err(match (backing == Backing::Slave, addr_port) {
                (true, Some(port)) => ContractError::MemoryDrivenByTheDut {
                    bundle: binding.bundle.clone(),
                    port: port.name.clone(),
                },
                _ => ContractError::MemoryRoleOnNonMemory {
                    bundle: binding.bundle.clone(),
                    port: found.port.clone(),
                    role: found.role.as_str(),
                    backing: backing.to_string(),
                },
            });
        }
    }

    // Transfer-level roles have meaning only on an addressable backing. The
    // port shape is the same for `bram` and `host_mem`.
    let addressable = matches!(
        manifest.bundle[&binding.bundle].backing,
        Backing::Bram | Backing::HostMem
    );
    if !addressable && let Some(found) = roles.iter().find(|r| r.role.is_host_mem()) {
        return Err(ContractError::HostMemRoleOnOther {
            bundle: binding.bundle.clone(),
            port: found.port.clone(),
            role: found.role.as_str(),
            backing: manifest.bundle[&binding.bundle].backing.to_string(),
        });
    }

    // AXI4 skips the contract check. It has five channels, each with its own
    // valid/ready, and the check assumes one handshake. AXI4 defines its own
    // flow control, so the contract holds.
    if roles.iter().any(|r| r.role == Role::Axi4) {
        if roles.len() > 1 {
            return Err(ContractError::Axi4NotAlone {
                bundle: binding.bundle.clone(),
                others: roles
                    .iter()
                    .filter(|r| r.role != Role::Axi4)
                    .map(|r| r.port.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }
        // The interface carries the contract. Any written value other than
        // `axi` would have no effect.
        if let Some(declared) = declared
            && declared != Contract::Axi
        {
            return Err(ContractError::ContractNotYours {
                bundle: binding.bundle.clone(),
                declared: declared.as_str(),
                inferred: Contract::Axi.as_str(),
                why: "the port is a std interface, and the protocol comes with it",
            });
        }
        return Ok(BundleContract {
            bundle: binding.bundle.clone(),
            contract: Contract::Axi,
            declared: declared.is_some(),
            roles,
        });
    }

    // A transfer-level port skips the contract check too. It has two
    // valid/ready pairs (commands and data), and the check assumes one.
    // Instead, check that the roles are complete. The contract is
    // `valid_ready`.
    if roles.iter().any(|r| r.role.is_host_mem()) {
        check_host_mem(&binding.bundle, &roles)?;
        if let Some(declared) = declared
            && declared != Contract::ValidReady
        {
            return Err(ContractError::ContractDisagrees {
                bundle: binding.bundle.clone(),
                declared: declared.as_str(),
                inferred: Contract::ValidReady.as_str(),
                roles: role_list(&roles),
            });
        }
        return Ok(BundleContract {
            bundle: binding.bundle.clone(),
            contract: Contract::ValidReady,
            declared: declared.is_some(),
            roles,
        });
    }

    // The contract follows from the roles, one to one. A written contract is
    // only a claim, and it is checked.
    let inferred = infer_contract(&roles).ok_or_else(|| ContractError::HalfHandshake {
        bundle: binding.bundle.clone(),
        roles: role_list(&roles),
    })?;
    // Do not fall into `valid_only` silently. An inverted ready (such as
    // `o_full`) is not in the dictionary, so it becomes payload and the
    // inferred contract is `valid_only`, which drops beats. Ask the user.
    if declared.is_none() && inferred == Contract::ValidOnly {
        let hint = inversion_hint(&roles);
        if !hint.is_empty() {
            return Err(ContractError::ContractRequired {
                bundle: binding.bundle.clone(),
                roles: role_list(&roles),
                hint,
            });
        }
    }
    if let Some(declared) = declared
        && declared != inferred
    {
        return Err(ContractError::ContractDisagrees {
            bundle: binding.bundle.clone(),
            declared: declared.as_str(),
            inferred: inferred.as_str(),
            roles: role_list(&roles),
        });
    }
    let contract = inferred;
    check_contract(dut, binding, &roles)?;

    Ok(BundleContract {
        bundle: binding.bundle.clone(),
        contract,
        declared: declared.is_some(),
        roles,
    })
}

/// Checks that a transfer-level bundle has all the roles it needs.
///
/// The contract check does not apply: there are two valid/ready pairs
/// (commands and data). Instead, each started side must have all 8 roles.
/// `*_tag` is optional.
fn check_host_mem(bundle: &str, roles: &[RoleAssignment]) -> Result<(), ContractError> {
    let has = |role: Role| roles.iter().any(|r| r.role == role);

    // Roles that are known but not generated yet. The error says "not yet",
    // not "unknown".
    // `wr_done_valid` is optional, so a DUT that does not wait for completion
    // still works. Without it, the DUT cannot know when its writes are
    // visible; that is the DUT author's choice.
    for (role, name) in [
        (Role::RdCmdTag, "rd_cmd_tag"),
        (Role::RdTag, "rd_tag"),
        (Role::WrCmdTag, "wr_cmd_tag"),
        (Role::WrDoneTag, "wr_done_tag"),
        (Role::WrDoneError, "wr_done_error"),
    ] {
        if has(role) {
            return Err(ContractError::HostMemRoleUnsupported {
                bundle: bundle.to_string(),
                role: name,
            });
        }
    }

    const READ: &[(Role, &str, &str)] = &[
        (
            Role::RdCmdValid,
            "rd_cmd_valid",
            "when a command is being offered",
        ),
        (
            Role::RdCmdReady,
            "rd_cmd_ready",
            "when a command was accepted",
        ),
        (Role::RdCmdAddr, "rd_cmd_addr", "where to read from"),
        (Role::RdCmdSize, "rd_cmd_size", "how much to read"),
        (Role::RdValid, "rd_valid", "when a beat may be handed over"),
        (Role::RdReady, "rd_ready", "when the DUT can take a beat"),
        (Role::RdData, "rd_data", "what to hand back"),
        (Role::RdLast, "rd_last", "how to say the transfer is over"),
    ];
    const WRITE: &[(Role, &str, &str)] = &[
        (
            Role::WrCmdValid,
            "wr_cmd_valid",
            "when a command is being offered",
        ),
        (
            Role::WrCmdReady,
            "wr_cmd_ready",
            "when a command was accepted",
        ),
        (Role::WrCmdAddr, "wr_cmd_addr", "where to write to"),
        (Role::WrCmdSize, "wr_cmd_size", "how much to write"),
        (Role::WrValid, "wr_valid", "when a beat is being offered"),
        (Role::WrReady, "wr_ready", "when the memory can take a beat"),
        (Role::WrData, "wr_data", "what to store"),
        (Role::WrLast, "wr_last", "when the DUT has no more beats"),
    ];

    // A DUT that only reads has no `wr_*`, and one that only writes has no
    // `rd_*`. Check only the sides that have at least one role.
    for side in [READ, WRITE] {
        let have = side.iter().filter(|(role, ..)| has(*role)).count();
        if have == 0 {
            continue;
        }
        if let Some((_, missing, why)) = side.iter().find(|(role, ..)| !has(*role)) {
            return Err(ContractError::HostMemIncomplete {
                bundle: bundle.to_string(),
                have,
                missing,
                why,
            });
        }
    }
    Ok(())
}

/// The policy: the dictionary, overridden by explicit roles. Anything left is
/// payload.
fn assign_roles(
    dut: &Dut,
    binding: &Binding,
    spec: Option<&PortSpec>,
    prefixes: &DirectionPrefixes,
    memory: bool,
) -> Vec<RoleAssignment> {
    // Explicit roles: port name -> (role, inverted).
    let explicit: BTreeMap<&str, (Role, bool)> = match spec {
        Some(PortSpec::Roles(roles)) => roles
            .iter()
            .map(|(role, target)| (target.port.as_str(), (*role, target.invert)))
            .collect(),
        _ => BTreeMap::new(),
    };

    binding
        .ports
        .iter()
        .map(|name| {
            if let Some((role, invert)) = explicit.get(name.as_str()) {
                return RoleAssignment {
                    port: name.clone(),
                    role: *role,
                    invert: *invert,
                    source: RoleSource::Explicit,
                };
            }

            let (role, source) = match dut
                .ports
                .iter()
                .find(|port| &port.name == name)
                .and_then(|port| role_from_name(&port.name, port.direction, prefixes, memory))
            {
                Some(role) => (role, RoleSource::Dictionary),
                // Payload. If a role the contract needs is missing, a later
                // check fails, so nothing passes silently.
                None => (Role::Data, RoleSource::Payload),
            };

            RoleAssignment {
                port: name.clone(),
                role,
                invert: false,
                source,
            }
        })
        .collect()
}

/// The suffix dictionary. It looks at the last `_` part of the name, after the
/// direction prefix is removed.
///
/// Keep it conservative. Adding `full` here would start inferring polarity
/// silently.
fn role_from_name(
    name: &str,
    direction: PortDirection,
    prefixes: &DirectionPrefixes,
    memory: bool,
) -> Option<Role> {
    let stripped = prefixes.strip(name, direction);
    let last = stripped.rsplit('_').next()?;
    if memory {
        // Only for memory terminators. Elsewhere `rdata` is payload; a name
        // alone must not turn a port into a memory port.
        match last {
            "addr" => return Some(Role::Addr),
            "rdata" => return Some(Role::RData),
            "wdata" => return Some(Role::WData),
            "we" => return Some(Role::We),
            "wstrb" | "be" => return Some(Role::Wstrb),
            // `en` is not guessed. In SRAM style, `en` means "access" and `we`
            // gives the direction; reading it as a read enable would be wrong.
            // To use it as one, name it in `ports`.
            "re" | "ren" => return Some(Role::Re),
            _ => {}
        }
    }
    match last {
        "valid" | "vld" => Some(Role::Valid),
        "ready" | "rdy" => Some(Role::Ready),
        "data" => Some(Role::Data),
        _ => None,
    }
}

/// Decides the contract from the set of roles, one to one.
///
/// Returns `None` for a `ready` without a `valid`: half a handshake fits no
/// contract, and the caller reports it.
fn infer_contract(roles: &[RoleAssignment]) -> Option<Contract> {
    let has = |role: Role| roles.iter().any(|assignment| assignment.role == role);
    // Transfer-level roles give `valid_ready`: the commands and the data
    // each have valid/ready.
    if roles.iter().any(|assignment| assignment.role.is_host_mem()) {
        return Some(Contract::ValidReady);
    }
    match (has(Role::Valid), has(Role::Ready)) {
        (true, true) => Some(Contract::ValidReady),
        (true, false) => Some(Contract::ValidOnly),
        (false, false) => Some(Contract::FixedLatency),
        (false, true) => None,
    }
}

/// Rejects two ports with one handshake role, and a valid and ready in the
/// same direction.
///
/// The contract comes from the roles, so a missing or forbidden role cannot
/// happen. A mismatch with a written contract is `ContractDisagrees`.
fn check_contract(
    dut: &Dut,
    binding: &Binding,
    roles: &[RoleAssignment],
) -> Result<(), ContractError> {
    for role in [Role::Valid, Role::Ready] {
        let ports: Vec<&str> = roles
            .iter()
            .filter(|assignment| assignment.role == role)
            .map(|assignment| assignment.port.as_str())
            .collect();
        if ports.len() > 1 {
            return Err(ContractError::DuplicateRole {
                bundle: binding.bundle.clone(),
                role,
                ports: ports.join(", "),
            });
        }
    }

    // One side drives valid and the other drives ready. In the same
    // direction, one of the roles is wrong.
    let (first, second) = (Role::Valid, Role::Ready);
    if let (Some(a), Some(b)) = (find_port(dut, roles, first), find_port(dut, roles, second))
        && a.direction == b.direction
    {
        return Err(ContractError::SameDirection {
            bundle: binding.bundle.clone(),
            first_role: first,
            first: a.name.clone(),
            second_role: second,
            second: b.name.clone(),
            direction: a.direction.as_str(),
        });
    }

    Ok(())
}

fn find_port<'a>(dut: &'a Dut, roles: &[RoleAssignment], role: Role) -> Option<&'a Port> {
    let name = roles
        .iter()
        .find(|assignment| assignment.role == role)
        .map(|assignment| assignment.port.as_str())?;
    dut.ports.iter().find(|port| port.name == name)
}

fn role_list(roles: &[RoleAssignment]) -> String {
    roles
        .iter()
        .map(|assignment| {
            format!(
                "    {:<16} {} ({})",
                assignment.port,
                assignment.role,
                assignment.source.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Names ports that look inverted. It infers nothing; it only shows how to
/// write the fix.
fn inversion_hint(roles: &[RoleAssignment]) -> String {
    let candidates: Vec<&str> = roles
        .iter()
        .filter(|assignment| assignment.source == RoleSource::Payload)
        .map(|assignment| assignment.port.as_str())
        .filter(|port| {
            let last = port.rsplit('_').next().unwrap_or(port);
            INVERSION_HINTS.contains(&last)
        })
        .collect();

    if candidates.is_empty() {
        return String::new();
    }

    format!(
        "\n`{}` looks like an inverted handshake signal (full = !ready, empty = !valid). The polarity is not guessed. If it is inverted, write it:\n\n    ports = {{ ready = \"!{}\", .. }}\n\n",
        candidates.join("`, `"),
        candidates[0],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::How;
    use crate::dut::{Domain, Signal, SignalRole};

    fn port(name: &str, direction: PortDirection) -> Port {
        Port {
            name: name.to_string(),
            direction,
            axi4: None,
            signals: vec![Signal {
                path: name.to_string(),
                role: SignalRole::Data,
                type_text: "logic<1>".to_string(),
                width: Some(1),
                array: Some(1),
                domain: Domain::None,
            }],
        }
    }

    fn dut(ports: Vec<Port>) -> Dut {
        Dut {
            name: "dut_top".to_string(),
            file: "src/dut_top.veryl".into(),
            line: 1,
            ports,
        }
    }

    fn binding(bundle: &str, ports: &[&str]) -> Binding {
        Binding {
            bundle: bundle.to_string(),
            ports: ports.iter().map(|port| port.to_string()).collect(),
            how: How::Naming,
        }
    }

    fn manifest(toml: &str) -> Manifest {
        toml::from_str(toml).unwrap()
    }

    // -----------------------------------------------------------------------
    // `host_mem`
    // -----------------------------------------------------------------------

    /// A transfer-level port with all 8 read roles.
    const HMEM_PORTS: &[&str] = &[
        "o_cmd_valid",
        "i_cmd_ready",
        "o_cmd_addr",
        "o_cmd_size",
        "i_rd_valid",
        "o_rd_ready",
        "i_rd_data",
        "i_rd_last",
    ];

    fn hmem_dut() -> Dut {
        dut(vec![
            port("o_cmd_valid", PortDirection::Output),
            port("i_cmd_ready", PortDirection::Input),
            port("o_cmd_addr", PortDirection::Output),
            port("o_cmd_size", PortDirection::Output),
            port("i_rd_valid", PortDirection::Input),
            port("o_rd_ready", PortDirection::Output),
            port("i_rd_data", PortDirection::Input),
            port("i_rd_last", PortDirection::Input),
        ])
    }

    fn hmem_manifest(roles: &str) -> Manifest {
        manifest(&format!(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.hmem]\nbacking = \"host_mem\"\nports = {{ {roles} }}\n"
        ))
    }

    const HMEM_READ_ROLES: &str = "rd_cmd_valid = \"o_cmd_valid\", rd_cmd_ready = \"i_cmd_ready\", \
         rd_cmd_addr = \"o_cmd_addr\", rd_cmd_size = \"o_cmd_size\", \
         rd_valid = \"i_rd_valid\", rd_ready = \"o_rd_ready\", \
         rd_data = \"i_rd_data\", rd_last = \"i_rd_last\"";

    /// It skips the contract check, which assumes one handshake. This port
    /// has two valid/ready pairs.
    #[test]
    fn a_transfer_level_read_port_resolves() {
        let resolved = resolve(
            &hmem_dut(),
            &hmem_manifest(HMEM_READ_ROLES),
            &[binding("hmem", HMEM_PORTS)],
            &DirectionPrefixes::default(),
        )
        .unwrap();
        let roles: Vec<Role> = resolved[0].roles.iter().map(|r| r.role).collect();
        assert!(roles.contains(&Role::RdCmdAddr), "{roles:?}");
        assert!(roles.contains(&Role::RdLast), "{roles:?}");
        // Tags are optional. `examples/hw/dma` has none.
        assert!(!roles.contains(&Role::RdCmdTag), "{roles:?}");
    }

    /// A missing role is named, with what cannot be said without it.
    #[test]
    fn a_read_port_missing_a_role_says_which_and_why() {
        // Make `rd_last` payload. The port is still terminated, so the
        // unterminated-port check does not fire first.
        let roles = HMEM_READ_ROLES.replace("rd_last = ", "data = ");
        let err = resolve(
            &hmem_dut(),
            &hmem_manifest(&roles),
            &[binding("hmem", HMEM_PORTS)],
            &DirectionPrefixes::default(),
        )
        .unwrap_err();
        // Do not check the rendered report: miette wraps lines and splits
        // words.
        let head = err.to_string();
        let help = miette::Diagnostic::help(&err)
            .map(|h| h.to_string())
            .unwrap_or_default();
        assert!(head.contains("rd_last"), "{head}");
        assert!(help.contains("the transfer is over"), "{help}");
    }

    /// Only a started side must be complete. One write role among the read
    /// roles starts the write side, and then it needs all of its roles.
    #[test]
    fn half_a_port_is_stated_whole_or_left_out() {
        let roles = HMEM_READ_ROLES.replace("rd_data = ", "wr_data = ");
        let err = resolve(
            &hmem_dut(),
            &hmem_manifest(&roles),
            &[binding("hmem", HMEM_PORTS)],
            &DirectionPrefixes::default(),
        )
        .unwrap_err();
        let head = err.to_string();
        let help = miette::Diagnostic::help(&err)
            .map(|h| h.to_string())
            .unwrap_or_default();
        // The read side lost `rd_data`, and the write side has one role. The
        // read side is checked first, so it reports.
        assert!(head.contains("rd_data"), "{head}");
        assert!(help.contains("only reads"), "{help}");
    }

    /// The completion is optional, so a DUT that does not wait for it works.
    #[test]
    fn a_write_without_a_completion_is_allowed() {
        let dut = dut(vec![
            port("o_wcmd_valid", PortDirection::Output),
            port("i_wcmd_ready", PortDirection::Input),
            port("o_wcmd_addr", PortDirection::Output),
            port("o_wcmd_size", PortDirection::Output),
            port("o_w_valid", PortDirection::Output),
            port("i_w_ready", PortDirection::Input),
            port("o_w_data", PortDirection::Output),
            port("o_w_last", PortDirection::Output),
        ]);
        let roles = "wr_cmd_valid = \"o_wcmd_valid\", wr_cmd_ready = \"i_wcmd_ready\", \
             wr_cmd_addr = \"o_wcmd_addr\", wr_cmd_size = \"o_wcmd_size\", \
             wr_valid = \"o_w_valid\", wr_ready = \"i_w_ready\", \
             wr_data = \"o_w_data\", wr_last = \"o_w_last\"";
        let ports = [
            "o_wcmd_valid",
            "i_wcmd_ready",
            "o_wcmd_addr",
            "o_wcmd_size",
            "o_w_valid",
            "i_w_ready",
            "o_w_data",
            "o_w_last",
        ];
        let resolved = resolve(
            &dut,
            &hmem_manifest(roles),
            &[binding("hmem", &ports)],
            &DirectionPrefixes::default(),
        )
        .unwrap();
        let got: Vec<Role> = resolved[0].roles.iter().map(|r| r.role).collect();
        assert!(got.contains(&Role::WrLast), "{got:?}");
        assert!(!got.contains(&Role::WrDoneValid), "{got:?}");
    }

    /// Tag and completion-detail roles are known but not generated yet.
    #[test]
    fn a_role_nothing_emits_yet_says_so() {
        let roles = format!("{HMEM_READ_ROLES}, rd_tag = \"o_cmd_size\"");
        let err = resolve(
            &hmem_dut(),
            &hmem_manifest(&roles),
            &[binding("hmem", HMEM_PORTS)],
            &DirectionPrefixes::default(),
        )
        .unwrap_err();
        let head = err.to_string();
        assert!(head.contains("rd_tag"), "{head}");
        assert!(head.contains("not generated yet"), "{head}");
    }

    /// Transfer-level roles have meaning only on `host_mem` and `bram`.
    #[test]
    fn a_transfer_role_on_another_backing_is_refused() {
        let bar = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.hmem]\nbacking = \"reg\"\nports = { rd_cmd_addr = \"o_cmd_addr\" }\n",
        );
        let err = resolve(
            &hmem_dut(),
            &bar,
            &[binding("hmem", &["o_cmd_addr"])],
            &DirectionPrefixes::default(),
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("reg"), "{text}");
        assert!(text.contains("rd_cmd_addr"), "{text}");
    }

    /// Regular names are resolved by the dictionary.
    #[test]
    fn the_dictionary_reads_the_suffix() {
        let dut = dut(vec![
            port("o_dmem_valid", PortDirection::Output),
            port("i_dmem_ready", PortDirection::Input),
            port("o_dmem_data", PortDirection::Output),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.dmem]\ncontract = \"valid_ready\"\nbacking = \"bram\"\n",
        );
        let bindings = vec![binding(
            "dmem",
            &["o_dmem_valid", "i_dmem_ready", "o_dmem_data"],
        )];

        let resolved = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap();

        assert_eq!(resolved[0].contract, Contract::ValidReady);
        assert!(resolved[0].declared);
        assert_eq!(resolved[0].roles[0].role, Role::Valid);
        assert_eq!(resolved[0].roles[0].source, RoleSource::Dictionary);
        assert_eq!(resolved[0].roles[1].role, Role::Ready);
    }

    /// The FIFO case. `full` is not in the dictionary, so it must be named.
    #[test]
    fn an_explicit_role_map_wins_and_carries_polarity() {
        let dut = dut(vec![
            port("i_push", PortDirection::Input),
            port("i_data", PortDirection::Input),
            port("o_full", PortDirection::Output),
        ]);
        let manifest = manifest(
            r#"
[dut]
module = "dut_top"

[bundle.push]
contract = "valid_ready"
backing  = "reg"
ports    = { valid = "i_push", ready = "!o_full", data = "i_data" }
"#,
        );
        let bindings = vec![binding("push", &["i_push", "i_data", "o_full"])];

        let resolved = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap();

        let ready = resolved[0]
            .roles
            .iter()
            .find(|assignment| assignment.role == Role::Ready)
            .unwrap();
        assert_eq!(ready.port, "o_full");
        assert!(ready.invert, "the ! prefix must survive");
        assert_eq!(ready.source, RoleSource::Explicit);
    }

    /// `o_full` is not guessed to be a `ready`: a name does not tell polarity.
    ///
    /// The inferred contract would be `valid_only`, which drops beats, so the
    /// user is asked instead.
    #[test]
    fn full_is_not_guessed_to_be_a_ready() {
        let dut = dut(vec![
            port("i_push_valid", PortDirection::Input),
            port("o_full", PortDirection::Output),
        ]);
        let manifest =
            manifest("[dut]\nmodule = \"dut_top\"\n\n[bundle.push]\nbacking = \"reg\"\n");
        let bindings = vec![binding("push", &["i_push_valid", "o_full"])];

        let err = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap_err();
        let ContractError::ContractRequired { hint, roles, .. } = &err else {
            panic!("expected ContractRequired, got {err:?}");
        };
        // `o_full` stays payload.
        assert!(roles.contains("o_full"), "{roles}");
        // The hint says it may be inverted.
        assert!(hint.contains("o_full"), "{hint}");
        let help = miette::Diagnostic::help(&err)
            .map(|h| h.to_string())
            .unwrap_or_default();
        assert!(help.contains("valid_only"), "{help}");
    }

    /// An omitted contract is not an error. It comes from the ports.
    #[test]
    fn an_omitted_contract_is_inferred_from_the_ports() {
        let dut = dut(vec![port("o_dbg_state", PortDirection::Output)]);
        let manifest =
            manifest("[dut]\nmodule = \"dut_top\"\n\n[bundle.dbg]\nbacking = \"observe\"\n");
        let bindings = vec![binding("dbg", &["o_dbg_state"])];

        let contracts = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap();
        // No handshake means `fixed_latency`.
        assert_eq!(contracts[0].contract, Contract::FixedLatency);
        assert!(!contracts[0].declared);
    }

    /// A written contract that disagrees with the ports is rejected. That is
    /// why writing one is useful.
    #[test]
    fn a_written_contract_that_disagrees_with_the_ports_is_rejected() {
        let dut = dut(vec![
            port("o_mem_addr", PortDirection::Output),
            port("i_mem_rdata", PortDirection::Input),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.mem]\ncontract = \"valid_ready\"\nbacking = \"bram\"\n",
        );
        let bindings = vec![binding("mem", &["o_mem_addr", "i_mem_rdata"])];

        let err = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap_err();
        let ContractError::ContractDisagrees {
            declared, inferred, ..
        } = &err
        else {
            panic!("expected ContractDisagrees, got {err:?}");
        };
        assert_eq!((*declared, *inferred), ("valid_ready", "fixed_latency"));
        // Show both values. Only the author knows which one is wrong.
        let rendered = format!("{:?}", miette::Report::new(err));
        assert!(rendered.contains("valid_ready"), "{rendered}");
        assert!(rendered.contains("fixed_latency"), "{rendered}");
    }

    /// A `ready` without a `valid` fits no contract.
    #[test]
    fn a_ready_without_a_valid_is_rejected() {
        let dut = dut(vec![
            port("o_mem_addr", PortDirection::Output),
            port("i_mem_ready", PortDirection::Input),
        ]);
        let manifest =
            manifest("[dut]\nmodule = \"dut_top\"\n\n[bundle.mem]\nbacking = \"bram\"\n");
        let bindings = vec![binding("mem", &["o_mem_addr", "i_mem_ready"])];

        let err = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap_err();
        assert!(
            matches!(err, ContractError::HalfHandshake { .. }),
            "got {err:?}"
        );
        let help = miette::Diagnostic::help(&err)
            .map(|h| h.to_string())
            .unwrap_or_default();
        assert!(help.contains("fixed_latency"), "{help}");
    }

    /// `valid_only` with a ready can be the safer `valid_ready`.
    #[test]
    fn valid_only_with_a_ready_is_rejected() {
        let dut = dut(vec![
            port("o_tx_valid", PortDirection::Output),
            port("i_tx_ready", PortDirection::Input),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.tx]\ncontract = \"valid_only\"\nbacking = \"host_poll_fifo\"\n",
        );
        let bindings = vec![binding("tx", &["o_tx_valid", "i_tx_ready"])];

        let err = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap_err();
        let ContractError::ContractDisagrees {
            declared, inferred, ..
        } = &err
        else {
            panic!("expected ContractDisagrees, got {err:?}");
        };
        assert_eq!((*declared, *inferred), ("valid_only", "valid_ready"));
    }

    /// valid and ready go in opposite directions, so the same direction is
    /// wrong.
    #[test]
    fn valid_and_ready_in_the_same_direction_are_rejected() {
        let dut = dut(vec![
            port("i_a_valid", PortDirection::Input),
            port("i_a_ready", PortDirection::Input),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.a]\ncontract = \"valid_ready\"\nbacking = \"reg\"\n",
        );
        let bindings = vec![binding("a", &["i_a_valid", "i_a_ready"])];

        let err = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap_err();
        assert!(
            matches!(err, ContractError::SameDirection { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn two_ports_with_the_same_role_are_rejected() {
        let dut = dut(vec![
            port("o_a_valid", PortDirection::Output),
            port("o_b_valid", PortDirection::Output),
            port("i_a_ready", PortDirection::Input),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.a]\ncontract = \"valid_ready\"\nbacking = \"reg\"\n",
        );
        let bindings = vec![binding("a", &["o_a_valid", "o_b_valid", "i_a_ready"])];

        let err = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap_err();
        assert!(
            matches!(err, ContractError::DuplicateRole { .. }),
            "got {err:?}"
        );
    }

    /// Memory names are looked up only for memory terminators. On `reg`,
    /// `rdata` is payload.
    #[test]
    fn the_memory_dictionary_only_applies_to_memory_backings() {
        let dut = dut(vec![
            port("o_mem_addr", PortDirection::Output),
            port("i_mem_rdata", PortDirection::Input),
        ]);
        let bindings = vec![binding("mem", &["o_mem_addr", "i_mem_rdata"])];

        let memory = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.mem]\ncontract = \"fixed_latency\"\nlatency = 2\nbacking = \"bram\"\n",
        );
        let resolved = resolve(&dut, &memory, &bindings, &DirectionPrefixes::default()).unwrap();
        assert_eq!(resolved[0].roles[0].role, Role::Addr);
        assert_eq!(resolved[0].roles[0].source, RoleSource::Dictionary);
        assert_eq!(resolved[0].roles[1].role, Role::RData);

        let csr = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.mem]\ncontract = \"fixed_latency\"\nlatency = 2\nbacking = \"reg\"\n",
        );
        let resolved = resolve(&dut, &csr, &bindings, &DirectionPrefixes::default()).unwrap();
        assert!(
            resolved[0]
                .roles
                .iter()
                .all(|assignment| assignment.role == Role::Data),
            "{:?}",
            resolved[0].roles
        );
    }

    /// A memory role written on a non-memory terminator is an error. It does
    /// not silently become payload.
    #[test]
    fn a_memory_role_on_a_non_memory_backing_is_rejected() {
        let dut = dut(vec![
            port("o_x", PortDirection::Output),
            port("i_y", PortDirection::Input),
        ]);
        // `host_poll_fifo` has no memory roles, so they are refused.
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.b]\ncontract = \"valid_only\"\nbacking = \"host_poll_fifo\"\nports = { addr = \"o_x\", rdata = \"i_y\" }\n",
        );
        let bindings = vec![binding("b", &["o_x", "i_y"])];

        let err = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap_err();
        assert!(
            matches!(err, ContractError::MemoryRoleOnNonMemory { .. }),
            "got {err:?}"
        );
    }

    /// On `slave`, the address direction decides.
    ///
    /// If the DUT drives the address, the DUT reads a memory; the host does not
    /// access the DUT. The error suggests `bram` by name, so the user knows
    /// what to change.
    #[test]
    fn an_address_the_dut_drives_on_slave_points_at_bram() {
        let dut = dut(vec![
            port("o_x", PortDirection::Output),
            port("i_y", PortDirection::Input),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.b]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"slave\"\nports = { addr = \"o_x\", rdata = \"i_y\" }\n",
        );
        let bindings = vec![binding("b", &["o_x", "i_y"])];

        let err = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap_err();
        assert!(
            matches!(err, ContractError::MemoryDrivenByTheDut { .. }),
            "got {err:?}"
        );
        let help = miette::Diagnostic::help(&err)
            .map(|h| h.to_string())
            .unwrap_or_default();
        assert!(help.contains("bram"), "{help}");
    }

    /// An address on `reg`: the error names the backing for each direction.
    #[test]
    fn an_addressable_port_on_reg_names_both_ways_out() {
        let dut = dut(vec![
            port("i_x", PortDirection::Input),
            port("o_y", PortDirection::Output),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.b]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"reg\"\nports = { addr = \"i_x\", rdata = \"o_y\" }\n",
        );
        let bindings = vec![binding("b", &["i_x", "o_y"])];

        let err = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap_err();
        let help = miette::Diagnostic::help(&err)
            .map(|h| h.to_string())
            .unwrap_or_default();
        assert!(help.contains("bram"), "{help}");
        assert!(help.contains("slave"), "{help}");
    }

    /// An address the DUT receives is an addressable slave port. It is
    /// accepted.
    #[test]
    fn an_address_the_dut_receives_on_slave_is_accepted() {
        let dut = dut(vec![
            port("i_x", PortDirection::Input),
            port("o_y", PortDirection::Output),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.b]\ncontract = \"fixed_latency\"\nlatency = 1\nbacking = \"slave\"\nports = { addr = \"i_x\", rdata = \"o_y\" }\n",
        );
        let bindings = vec![binding("b", &["i_x", "o_y"])];

        let contracts = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap();
        let roles = &contracts[0].roles;
        assert!(
            roles
                .iter()
                .any(|r| r.role == Role::Addr && r.port == "i_x")
        );
        assert!(
            roles
                .iter()
                .any(|r| r.role == Role::RData && r.port == "o_y")
        );
    }

    /// `fixed_latency` passes with no handshake: a payload-only bundle.
    #[test]
    fn fixed_latency_passes_with_payload_only() {
        let dut = dut(vec![
            port("o_mem_addr", PortDirection::Output),
            port("i_mem_rdata", PortDirection::Input),
        ]);
        let manifest = manifest(
            "[dut]\nmodule = \"dut_top\"\n\n[bundle.mem]\ncontract = \"fixed_latency\"\nlatency = 2\nbacking = \"reg\"\n",
        );
        let bindings = vec![binding("mem", &["o_mem_addr", "i_mem_rdata"])];

        let resolved = resolve(&dut, &manifest, &bindings, &DirectionPrefixes::default()).unwrap();

        assert_eq!(resolved[0].contract, Contract::FixedLatency);
        // Names the dictionary does not know are payload, never handshake.
        assert!(
            resolved[0]
                .roles
                .iter()
                .all(|assignment| assignment.role == Role::Data)
        );
        assert_eq!(resolved[0].roles[0].source, RoleSource::Payload);
    }
}
