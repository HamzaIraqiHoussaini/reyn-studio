# Professional study workflow and remembered access

Reyn adopts the parts of a professional CFD study environment that its current
solver and evidence contracts can support honestly.

## Study workflow

Case Setup is organized as:

1. Geometry
2. Qualified Model
3. Flow Conditions
4. Review Focus
5. Run Readiness

Readiness is represented by structured blockers with a stable code, owning
stage, and operator-facing message. Selecting a blocker returns Case Setup to
the stage that owns the unresolved control.

The 3D model selector is capability-driven:

- inventory loading and failure are explicit;
- a single compatible model is bound only to a new, unbound draft;
- multiple compatible models produce a real selector;
- a missing stored model keeps its recorded identity and blocks execution;
- the bundled 2D H64 model is never presented as a 3D option.

Research 3D evidence is the 32³ one-jump obstacle operator (RelL2 0.052 at 128
steps, 32,768 cells). CAD Run voxels the case to that declared grid. A missing
or mismatched signed bundle keeps Run disabled with that reason.

Qualification requires a canonical `.reynmodel` identity, verified publisher
authenticity, CLEAN status, 3D geometry conditioning at the model's declared
grid (32³ for the current operator), a 4→3 or 5→3 channel contract, fixed-body
obstacle scenario, `fixed_body_brinkman.v1`, a supported horizon, and a canonical
checkpoint SHA-256.

Review Focus is versioned in the case contract. It can lead with supported force
or moment components, Cp extrema, or wake-deficit quantities. It changes Results
and comparison emphasis only; it never claims to control convergence.

## Remembered access

The desktop never stores a username or password. Login exchanges those values
for a random opaque server session:

- Supabase stores only the token digest and revocation metadata.
- Session creation locks and rechecks the linked access code, so a concurrent
  credential revocation cannot leave a newly usable session behind.
- macOS stores the raw session in Keychain.
- Windows stores the raw session in Credential Manager.
- Linux stores the raw session in the Freedesktop Secret Service when available;
  if the service is missing, credential save/load fails closed with no plaintext
  fallback.
- settings and project files contain no access material.
- Unsupported targets have no plaintext fallback.

Startup validates a saved session before opening Studio. A revoked, expired,
malformed, or legally outdated session is deleted and returns to login.
Temporary network or service failures retain the secure session and offer
retry without asking for credentials again. Sessions are periodically
revalidated, and Sign Out revokes the server session before deleting the local
copy. If the service cannot confirm Sign Out, Reyn retains the token only in
the native vault and offers a retry instead of silently abandoning a live
server session.

## Deliberately not copied from SOLIDWORKS Flow Simulation

Reyn does not expose controls that its current model does not support:

- generic internal, thermal, or rotating-flow setup;
- arbitrary mesh controls;
- residual or convergence theater;
- trajectories synthesized without a model-derived field.

The adopted pattern is the persistent study outline, explicit verification,
named outputs, visible stale/error states, real run progress, and reusable
result views—not another product's visual design or unsupported solver surface.
