# Reyn Studio 0.4.0 release notes

**Release:** 0.4.0  
**Channel:** invitation-only research preview  
**Platforms:** Apple-silicon macOS, Windows x64, and Ubuntu 24.04 x86_64 Linux

## Linux preview

This release publishes the first Ubuntu 24.04 x86_64 packages: an AppImage, a
portable `.tar.xz`, and a thin `.deb` under `/opt/reyn-studio`. The payload
bundles CPython, NumPy, and a CPU-only PyTorch runtime. CUDA and ROCm are not
supported or qualified.

Automated clean-machine checks on the published artifacts pass (digest, extract,
access contract, bundled engine, AppImage gate, `.deb` install, Secret Service
fail-closed). Interactive viewport, project I/O, STEP/VTK GUI, and Secret
Service-with-agent rows still need a desktop Ubuntu machine. Keep the public
label **Linux preview pending verification** until those rows are attested.

## STEP import honesty

- Multi-shell STEP pick-one now stores the selected shell entity id on the
  import and preflight records and passes it through orientation re-voxelize
  and stored-case hydrate, so a later attitude change does not reopen the
  chooser.
- Vendor-export fixtures (NIST CTC/FTC/STC plus Fusion, Onshape, and SolidWorks
  AP214) are in the qualification corpus. Native NX `.prt` / SolidWorks
  `.SLDPRT` files are staged for re-export only; Studio does not read them.

## Results chrome

- Project schema chrome uses the live schema version on both rails.
- Geometry status names transform approval or acceptance instead of leaving a
  sticky “preflight required” after the gate is accepted.
- Run finish copy no longer claims Q iso is on; the iso stays off by default
  on 64³ Brinkman fields and is available under Layers.
- Horizon status uses the model support span, matching playback.
- Reopened runs encode the GPU occupancy mask with the same 0.2 display floor
  as live results.

## Engineering evidence

Off-UI-thread CAD-field packaging, VTK smoke helpers, and Linux Secret Service
credential storage are included. Research preview only — consequential results
still require independent validation.
