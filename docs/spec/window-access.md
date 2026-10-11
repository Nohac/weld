# Window access, control sessions and peek

## Use cases — Direction

Weld should expose scoped application control for automation, guest and assist
sessions through the same window, input and presentation primitives as hoisting.

- Agent-led development: build and launch an application, inspect its frames,
  resize and exercise its input without taking the desktop user's focus.
- User-led development: an application occupies a persistent slot and hot
  reloads; the agent may observe it and explicitly acquire authorized input
  control when required.
- GUI assistance: the user marks an existing application as accessible and
  chooses what a participant may see and operate.

## Authority and composable window properties — Direction

Keep participant identity, grants, active control and presentation independent.
The durable managed window can carry persistence, access and peek preferences
together. Its current occupant remains a shorter-lived application surface.
Owning systems apply typed commands and validate lifecycle transitions; external
callers address stable Weld identifiers.

Grants distinguish viewing, keyboard, pointer/touch, viewport configuration,
close and launch authority. The initial policy allows multiple viewers and one
input controller per window. One logical input seat belongs to each control
session across its windows; local desktop input retains its own seat. Existing
single-seat application arbitration remains necessary when several windows
share one application connection. Window access does not isolate filesystem or
OS-user authority; stronger isolation requires a separate application environment.

Grant eligibility and input handoff are separate actions. A grant lets a
participant request control; it need not interrupt the user immediately.
Compositor-owned controls should show current ownership and offer View,
Keyboard and Pointer toggles and unconditional local Take Back. A header can
reuse tab/stack decoration styling. Protected system surfaces, including lock
and authorization UI, stay outside ordinary application grants.

Default grants address the current occupant and expire on its retirement.
Explicit persistent-slot grants may follow authorized replacement occupants.
Each occupation has an incarnation so queued input and captures for an old app
cannot cross into its replacement. Persistence matching chooses a candidate
slot; app IDs and titles alone do not prove that a replacement is authorized.
An occupied slot keeps its existing occupant unless an explicit action changes it.

Broad access to all applications requires explicit local authorization showing
participant, capabilities, existing/future-window scope and lifetime. Default to
session-limited authorization. MCP clients receive only the scoped operations
and credentials needed for their participant, separately from local admin RPC.
Authorization is checked when operations execute and before captured content is
delivered, including after asynchronous work completes.

## Launch — Direction

`weldctl launch -- PROGRAM ARGUMENTS...` starts an application through the running
Weld host, using its Wayland/XWayland endpoints and session environment. Ordinary
launch uses ordinary placement and focus policy. A launch associated with an
authorized control session can remain off-desktop or fill an eligible persistent
slot without stealing local focus. This uses the existing compositor and shared
client runtime, with consumer-driven rendering and frame callbacks.

Keep launch intent, control ownership and local presentation separate. A slot's
auto-peek preference determines whether a controlled occupant appears locally;
the same preference applies to hot reload and remote hoisting. Launch context
must be established before new windows enter management so focus and permissions
are correct on first admission.

Distinguish process-spawn completion, window association, first renderable frame
and requested viewport settlement. Return a launch identity and report failures
at the appropriate stage. Single-instance applications may delegate to an older
process; ambiguous association must be reported rather than granting unrelated
windows access based on a PID or app ID guess.

Agent launches of approved application entries and arbitrary executable launches
have separate permissions. Development sessions may explicitly authorize the
latter. Retain exact argument boundaries and an explicit working directory.

## Peek — Direction

A retained desktop frame can show a live, read-only view of an agent-controlled
or hoisted application. The active controller/presenter owns the application
viewport size, scale and cadence. Peek owns its display transform and clipping.
Resizing the retained frame never configures the application or changes its
authoritative output membership or preferred scale.

Default to contain: uniformly fit the entire content while preserving aspect
ratio, with background in unused space. A clip mode displays at a chosen
magnification within the frame; panning can follow later. Frame decoration and
controls retain desktop scale. Popups use the authorized application's composed
view and stay contained by the peek. Reuse native buffers for local presentation.

Peek itself acquires no input control. User interaction with its local controls
does not reach the remote app. Explicit Take Back ends external ownership and
restores desktop viewport authority through the shared reclaim lifecycle. App
exit restores the persistent placeholder; an authorized replacement can resume
peek. Visible peek contributes bounded presentation demand; hidden views pause
their own demand while other consumers remain independent.

## Emergency revocation — Direction

Provide a local `weldctl access revoke --all` operation and a bindable Weld
command, alongside window- and participant-scoped revocation. Global revocation
must invalidate external grants, stop new frame delivery, cancel queued work,
end control sessions, release held input/captures and reclaim hoisted windows.
Applications remain running and durable slots/layouts survive. Already delivered
images cannot be withdrawn.

The local emergency shortcut remains reachable during external control and
exclusive fullscreen. Participants cannot change it or authorize themselves.
Revocation latches access off: saved rules, pairing reconnection and persistent
replacement must not silently restore grants. An explicit local authorization
is required to resume access. Enforcement precedes potentially delayed visual
reclaim/resize settlement. Disconnect and session retirement release held keys,
buttons, contacts and grabs through the existing input cleanup boundary.

## Native control API and MCP — Direction

Extend Weld's typed local control API and shared domain systems. `weldctl` and
an MCP adapter consume that API; the host enforces grants, target incarnations
and input ownership. MCP should initially use stdio and a supported SDK rather
than making the compositor own agent-protocol transport.

Expose a stable set of operations: list accessible windows/slots, inspect,
acquire/release control, submit bounded typed input, resize, capture, authorized
launch and wait for lifecycle events. Listings distinguish supported operations,
granted permissions and current ownership. Capture returns the image plus frame
identity, geometry revision, dimensions, scale and coordinate mapping for input.
Use on-demand synchronized snapshots and the shared composed surface path;
native consumers can reuse buffer leases while MCP returns encoded still images.

The native event stream covers appearance/closure, first frame, viewport
settlement, grant changes, control revocation and occupant replacement. Bounded
history with an event cursor supports race-free waits and explicit resnapshot
after a history gap. Bounded cancellable waits provide reliable tool workflows.
MCP resource notifications can complement waits where supported; agent hosts
decide whether a notification starts another model turn. First-frame readiness
does not establish application-specific loading completion.

## Implementation sequence — Direction

1. Establish native launch RPC through the existing host child lifecycle and
   validate ordinary launches in presented and unpresented sessions.
2. Add scoped control-session authority and window inspection, including occupant
   incarnations, grants, controller handoff and emergency revocation. Connect
   existing hoist admission to this same authority before exposing agent input.
3. Add authorized launch association and persistent-slot replacement policy,
   with explicit reporting for unsupported single-instance delegation.
4. Expose input, viewport configuration, on-demand capture and bounded event
   waiting through the native API and CLI; test isolation and stale operations.
5. Add independent peek presentation and local access controls, preserving
   application viewport authority as the retained frame changes.
6. Add the MCP adapter over the validated native operations and exercise an
   application-development workflow end to end.

Each slice should record its implemented scope and validation separately from
these intended contracts. Real tests should include two applications with
independent input, shared-process terminals, popup capture, close/relaunch in a
persistent slot, session loss, stale asynchronous work and emergency revocation.
