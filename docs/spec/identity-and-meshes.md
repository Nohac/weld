# Identity, devices, and meshes

## Scope and terminology — Direction

Weld should distinguish authenticated network endpoints from the people,
groups, and resources they may represent. The identity model is independent of
Iroh, hoisting, Bevy, Smithay, and any particular wallet UI.

- A **mesh** is an independently administered trust and resource-sharing
  domain. One device or member may participate in several meshes.
- A **member** is a future human or service principal recognized by a mesh.
- A **device** is a cryptographic endpoint belonging to a member or service.
- A **resource** is something a mesh may disclose or operate, such as a device,
  application catalog entry, launch profile, window, workspace, media source,
  or game session.
- A **grant** authorizes specific capabilities over a resource.
- A **role** is a reusable policy that may produce a set of grants.
- An **invitation** is a bounded request to admit a device or member.
- An **authority** is a member or device permitted to approve or revoke some
  mesh state.

[**Window family**](remote-hoisting.md#scope--direction) remains the term for a
root Wayland toplevel and its related windows in the hoisting model. A
household mesh describes people sharing resources and is not another meaning
of family.

## Trust and authorization — Direction

Transport authentication proves which device endpoint established a
connection. It does not by itself establish a human identity, mesh membership,
or permission to discover, launch, hoist, control, or transfer data. Those
decisions come from explicit mesh authorization.

Each device owns a distinct private key. Weld must not distribute one shared
identity key to every device. Device-specific identities allow one compromised
or lost endpoint to be revoked without replacing every remaining device.
Authorization records must identify their issuer, subject, resource,
capabilities, validity, and revocation state in a form peers can authenticate.
The exact credential and replication format is not selected.

Access is capability-oriented rather than equivalent to unrestricted device
control. Consuming subsystems define the operations they protect: remote
presentation already separates
[launcher and hoist
capabilities](remote-hoisting.md#launcher-federation--direction)
from
[input and transferred-data
capabilities](remote-hoisting.md#security-and-recovery--direction).
Mesh administration and membership changes are independently grantable
mesh-owned capabilities. Mesh policy supplies the principals and grant
decisions without duplicating each subsystem's vocabulary. A role is a policy
convenience; the resulting effective grants remain inspectable and revocable.

Trusted peers should continue authorized direct operation while the phone or
other preferred administration device is offline. The management experience
must not make that device an always-online coordinator, mandatory traffic
relay, or single point of runtime availability.

Mesh membership does not create a local operating-system account, Wayland
seat, or multi-user compositor session. Mapping remote members onto seats,
accounts, containers, or sandboxes requires a separate explicit policy.

## Device wallet and pairing — Exploration

A mobile Weld application could be the primary **device wallet**: the
authoritative user experience for creating and switching meshes, enrolling
devices, reviewing grants, approving sensitive actions, observing sessions,
and revoking access. Authoritative here describes administration and user
intent, not transport topology. Another authorized device should be able to
perform administration and recovery.

The initial enrollment experience could use a short-lived QR invitation:

1. A candidate device displays an invitation containing enough information to
   establish a mutually authenticated pairing channel.
2. The wallet scans it and both endpoints present matching confirmation
   information.
3. An existing mesh authority approves the candidate and selects its initial
   member association, role, and capabilities.
4. The resulting authorization becomes available to other relevant trusted
   devices without making the wallet a proxy for their connections.

Invitation possession is not sufficient authorization. Invitations must be
time-bounded, single-purpose, bound to the candidate device identity, and
explicitly accepted by an existing authority. QR encoding, proximity checks,
biometric confirmation, and platform secure-key storage remain implementation
choices to validate on each client platform.

## Multiple meshes and shared resources — Exploration

The wallet may manage several overlapping domains, such as a personal mesh, a
household mesh, and a work mesh. A device may expose different resources and
hold different capabilities in each one without merging their membership,
discovery, or audit state.

Future member identities make it possible to grant a person access from more
than one of their devices. For example, a household gaming server could expose
an approved game catalog and permit selected members to discover games, launch
them, hoist their resulting windows, receive audio, and send gamepad input. It
need not grant shell access, general application discovery, filesystem access,
clipboard access, or permission to administer other members.

Resources should be addressed independently from the device currently hosting
them where practical. This allows a launcher or wallet to present one
authorized catalog while still disclosing where execution occurs and which
security boundary receives input or data.

## Revocation, recovery, and governance — Exploration

Membership and grants need authenticated propagation, conflict handling, and
revocation semantics that work across intermittently connected peers. A
signed, replicated authorization history is one candidate, but its ordering,
compaction, convergence, and compromise-recovery rules require separate
design. Cached authorization must not turn an expired or revoked broad grant
into indefinite offline access.

Losing the preferred wallet must not destroy the mesh. Candidate recovery
mechanisms include another administrator device, an offline recovery artifact,
or a threshold of existing authorities. The design must also distinguish lost
device recovery from a compromised administrator, ownership transfer, member
departure, and complete mesh deletion.

## Open work — Exploration

- Decide how member identities relate to device identities without requiring a
  centralized account service.
- Define who may issue, delegate, attenuate, and revoke each class of grant.
- Select an authenticated authorization record and replication model.
- Define discovery privacy between meshes and for unauthorized resources.
- Design guest access, expiry, approval prompts, and auditable session history.
- Validate device-key storage, migration, revocation, and recovery on Linux,
  Android, browsers, and other destination platforms.
