# Native CAD sources (not imported by Reyn)

These are **NIST CTC-01** parts in vendor-native formats. Reyn Studio does not
read `.SLDPRT` / `.prt`. They exist so a SolidWorks or NX operator can open the
file and export an honest single-body STEP into the parent `vendor/` folder.

| Path | Product | Role |
|---|---|---|
| `solidworks-mbd-2018/nist_ctc_01_asme1_rd_sw1802.SLDPRT` | SolidWorks MBD 2018 | Export → `../solidworks_nist_ctc01_ap214.step` (or AP203) |
| `nx-1980/nist_ctc_01_asme1_nx1980_rd.prt` | Siemens NX 1980 | Export → `../nx_nist_ctc01_ap242.step` (or AP214) |

Source pack: [NIST FTC/CTC PMI CAD models](https://www.nist.gov/document/nist-ftc-ctc-pmi-cad-models).

Export rules: single body, no assembly, record exporter version + STEP schema in
`../README` inventory (parent corpus README).
