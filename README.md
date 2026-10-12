<p align="center">
  <img src="icons/icon.png" width="112" alt="RAZE logo">
</p>

<h1 align="center">RAZE</h1>

<p align="center">
  <strong>Point-and-shoot removal for stubborn Windows files and folders.<br>Reserved names, 260-character walls, locked handles, broken ACLs — gone.</strong>
</p>

<p align="center"><a href="README.md"><strong>English</strong></a> · <a href="README.ru.md">Русский</a></p>

<p align="center">
  <a href="LICENSE"><img alt="GPL-3.0-or-later" src="https://img.shields.io/badge/license-GPL--3.0--or--later-2f855a"></a>
  <a href="https://t.me/rercon"><img alt="Telegram" src="https://img.shields.io/badge/Telegram-@rercon-26A5E4?logo=telegram&logoColor=white"></a>
</p>

<p align="center">
  <a href="https://github.com/RERCON0/RAZE/releases"><strong>Download for Windows</strong></a> ·
  <a href="#installation">Installation</a> ·
  <a href="#development">Build from source</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="SECURITY.md">Security</a>
</p>

<table>
  <tr>
    <th>RAZE application (English)</th>
    <th>Windows 11 context menu</th>
  </tr>
  <tr>
    <td width="50%" align="center" valign="top">
      <a href="docs/screenshot.png"><img src="docs/screenshot.png" width="420" alt="RAZE application with its English interface and synthetic demo files"></a>
    </td>
    <td width="50%" align="center" valign="top">
      <a href="docs/context-menu.png"><img src="docs/context-menu.png" width="420" alt="Modern Windows 11 COM context menu for demo-file.txt with Delete with RAZE"></a>
    </td>
  </tr>
</table>

<p align="center"><em>Screenshots use synthetic demo data only. Click an image to view it at full size.</em></p>

> [!TIP]
> **One click in Explorer, done.** Install the context-menu integration
> (`.\raze.exe --install` from the extracted folder; see [Installation](#installation)),
> right-click any stubborn folder or file, and choose
> **Delete with RAZE**. For anything headed to the Recycle Bin the click is the
> confirmation: nothing else is asked and the main window does not open. Objects
> that can only be deleted permanently ask for confirmation first. The UAC
> prompt appears only when the object really needs admin rights.

> [!NOTE]
> The `raze` CLI and `raze-app` GUI share the same engine
> (`raze_rs::job`): both decide identically what counts as a problem
> and where an object goes — Recycle Bin or permanent removal.

## What RAZE removes

Windows itself refuses these, and so does every normal tool:

| Obstacle | How RAZE gets past it |
|---|---|
| Reserved names (`NUL`, `CON`, `PRN`, `AUX`, `COM1`–`COM9`, `LPT1`–`LPT9`) | Extended paths (`\\?\...`) switch off name parsing, so `NUL` becomes an ordinary filename |
| Paths longer than 260 characters | Extended paths lift the `MAX_PATH` limit |
| Files held open by other programs | Restart Manager identifies holders; optionally `WM_CLOSE` to ordinary applications (`-c`) |
| Ownership and ACL walls | Takes ownership and grants access — only after an actual `ACCESS_DENIED` |
| Junctions, symlinks, other reparse points | Deletes the **link**, never follows or traverses it |
| `ERROR_DELETE_PENDING` objects | Retries until the object is really gone |

Permanent deletion goes through **handles** (`CreateFileW` +
`SetFileInformationByHandle` with `FILE_DISPOSITION_INFO_EX`), not path-based
delete APIs. Each opened object is checked before deleting that same handle.
The Recycle Bin uses the shell's path-based API; that operation is not atomic
with RAZE's preceding path checks.

**Honest dispositions:** recyclable objects go to the Recycle Bin and can be
restored; objects the bin cannot hold (reserved names, extended paths) are
removed permanently — and the report says which happened. The check runs
*before* deletion, because the shell silently deletes permanently when the
bin is full. For the same reason RAZE reads the drive's Recycle Bin settings
("don't move files to the Recycle Bin", the size limit, the policy) and plans
such objects as permanent instead of reporting a Recycle Bin that never
happened.
The shell's completion callbacks determine the actual disposition; merely
queueing an operation is not success. A Recycle Bin refusal — cancellation or
any other shell error — leaves the target in place: without explicit consent
(`--permanent-on-bin-failure`, or the dialog in the GUI or the Explorer menu)
nothing is deleted permanently in its stead. Moving to the bin does not count as freed space;
permanent-delete byte counts are logical-size estimates, not measurements of
filesystem allocation.

**Safety rails:** volume roots (`C:\`), the system folders themselves
(`C:\Windows`, `C:\Program Files`, `C:\ProgramData`, your profile and their
parents) and the RAZE program folder are refused outright. So is most of what
lies *inside* them: under `C:\Windows` only throwaway folders may be removed
(`Temp`, `Logs`, `Minidump`, `SoftwareDistribution`, `Panther`, `Debug`,
`Prefetch`, the print queue); in `Program Files` the folders that belong to
Windows itself (`WindowsApps`, `Windows *`, `Common Files`); on the system
drive `System Volume Information`, `Recovery`, `Boot`, `EFI`. A path is checked
twice — as typed and as it really lies on disk — so `C:\PROGRA~1` and a path
through a junction into `C:\Windows` are caught too, and the check is repeated
right before deletion. Paths with `* ? " < > |` or an NTFS stream (`:`) are
rejected: the shell would read the first as a mask.
Protected locations come from Windows APIs, not mutable environment variables.
If the safety anchors or the opened root's final path cannot be obtained,
removal fails closed. Native UTF-16 paths are retained throughout deletion,
including names that cannot be represented losslessly as Unicode text.

While a tree is being removed every object is verified, through the very
handle that is about to be deleted, to lie inside the target. If a folder was
swapped for a junction in the meantime, the object is skipped and reported, not
deleted.

## Features

- **Two context menus:** the classic registry verb for every Windows, plus the
  modern Windows 11 menu via a COM `IExplorerCommand` shell extension
  (`raze_shell.dll`) and a sparse MSIX package.
- **Point-and-select:** pick individual files and folders, not only whole trees.
- **Preview mode** (`-n`): see the plan — files, folders, links, bytes, problems —
  change nothing.
- **Deferred removal:** `--reboot` schedules objects for deletion at the next
  Windows restart — for what cannot be removed right now (Session Manager
  entries; needs administrator rights).
- **Elevation on demand:** unprivileged first; a UAC relaunch happens only on
  real `ACCESS_DENIED`.
- **GUI and CLI, one brain:** egui window with drag-and-drop, per-target
  checkboxes, blocker badges, dark/light themes, EN/RU.
- **Machine-readable output:** `--json` for scripts, `--plan-only` for a dry plan.

## CLI or GUI

| | `raze` | `raze-app` |
|---|---|---|
| Interface | Terminal | Native window |
| Unattended use | `-y` and arguments | Fields and buttons |
| Adding targets | Arguments, also `%1` from Explorer | Drag-and-drop, browser, typed path |
| Locked-file hunt | `-L` lists holders | `close locks` checkbox |
| Language | `--lang en` / `--lang ru` | EN / RU button in the title bar |

## Installation

### Download and install the Explorer menu

1. Download **raze-windows-x64.zip** from [Releases](https://github.com/RERCON0/RAZE/releases).
2. Extract the **whole archive** into a folder. Keep `raze.exe`, `raze-app.exe`,
   `raze_shell.dll`, `RAZE.msix` and `RAZE.cer` together; do not run them from inside the ZIP.
3. Open that folder in Explorer, right-click an empty area and choose **Open in Terminal**.
   Select **PowerShell** if your terminal opens a different shell.
4. Run the command below and confirm the Windows administrator prompt:

```powershell
.\raze.exe --install
```

**The `.\` prefix means “run the executable in this folder”.** PowerShell does
not search the current folder for a bare `raze` command, and RAZE's installer
does not add it to `PATH`. Keep using `.\raze.exe` from the extracted folder,
or specify the installed executable's full path:

```powershell
& "$env:ProgramFiles\RAZE\raze.exe" --help
```

The install asks for administrator rights **once** (one UAC prompt), copies
RAZE into `%ProgramFiles%\RAZE` and registers **Delete with RAZE** in Explorer.
Open a new Explorer window to check the entry on a file or folder. If the menu
has not refreshed, sign out of Windows and back in.

The download has an Ed25519-signed manifest; see [release verification](docs/RELEASING.md).
The EXEs have no Authenticode signature, so SmartScreen may show **Windows protected
your PC**. For a trusted download from the official Releases page, choose **More info →
Run anyway**. The ZIP signature and self-signed MSIX do not remove that warning.

### What installation changes

The install folder is writable only by administrators, which
matters: elevation re-runs `raze.exe`, and a copy in a folder any of your own
programs can overwrite could be swapped for something else before the prompt.
An earlier copy in `%LOCALAPPDATA%\Programs\RAZE` is cleaned up with the original
user's unelevated rights, never by elevated traversal of that user-writable
tree. If that token is unavailable, the installer requests manual cleanup.

The same run writes the classic verbs to `HKCU` and, when `RAZE.msix` and
`RAZE.cer` are next to `raze.exe` (see [Development](#development)), registers
the **modern Windows 11 menu** — the one that opens on a plain right-click,
where the entry reads **Delete with RAZE** (**Удалить с помощью RAZE** on a
Russian Windows). The classic verb lives under *Show more options*.

Windows only accepts menu extensions from signed packages. RAZE's package is
signed with a self-signed certificate (`CN=RERCON RAZE`), so the install adds
its **public** part to `LocalMachine\TrustedPeople` — and only if its
thumbprint matches the one compiled into `raze.exe` (`packaging\cert-thumbprint.txt`).
No private key is involved. To repeat just that package-registration step:

```powershell
& "$env:ProgramFiles\RAZE\raze.exe" --register-package
```

(If you would rather not trust a certificate,
skip `RAZE.msix`/`RAZE.cer`: the classic menu entry works without them.)

### Update or uninstall

To update, extract the new release into a separate folder and run
`.\raze.exe --install` **from that new folder**. Close the RAZE application first.

To uninstall, run this from the extracted folder:

```powershell
.\raze.exe --uninstall
```

If you no longer have that folder, use the installed copy from any PowerShell window:

```powershell
& "$env:ProgramFiles\RAZE\raze.exe" --uninstall
```

This removes the package, classic menu entries, trusted public certificate and
installed files with one UAC prompt. When uninstalling from the installed copy,
its running executable is removed after it exits. Your downloaded ZIP and extracted
folder are left in place.

The extension never deletes anything itself — it just launches `raze.exe` with
the selected paths, so a crash in Explorer can never corrupt a deletion in
progress. `raze_shell.dll` and `raze.exe` must stay in the same folder.

## Quick start

For portable use, double-click `raze-app.exe` to open the GUI; installing the
Explorer menu is optional. Run the CLI examples below in PowerShell **from the
extracted folder**. Replace each example path with your own target.

```powershell
# Preview what would happen
.\raze.exe -n "C:\temp\raze-demo"

# Remove a folder with a NUL inside — Explorer cannot even select it
.\raze.exe "C:\temp\raze-demo"

# Permanent removal, no Recycle Bin
.\raze.exe --permanent "C:\temp\old-build"

# Close programs that hold the files, then remove
.\raze.exe -c "C:\temp\locked"

# Schedule removal for the next Windows restart
.\raze.exe --reboot "C:\temp\locked"

# Who holds this file?
.\raze.exe -L -n "C:\temp\still-in-use.txt"
```

Multiple paths at once; quote paths with spaces. Without `-y` RAZE asks for
confirmation and prints a plan first — from the context menu it does not
(your click is the confirmation), except for objects that can only be deleted
permanently: those get a confirmation dialog.

| Argument | Purpose |
|---|---|
| `PATH` | Files and folders to remove |
| `-n, --dry-run` | Show what would be removed, change nothing |
| `-p, --permanent` | Remove permanently, bypassing the Recycle Bin |
| `--recycle` | Try the Recycle Bin where supported (the default); never bypass safety refusals |
| `--reboot` | Delete at the next Windows restart instead of now (Session Manager entry; needs administrator rights) |
| `--permanent-on-bin-failure` | Erase permanently if the Recycle Bin refuses an item (by default the target stays) |
| `-y, --yes` | Do not ask for confirmation |
| `-c, --close-locks` | Send `WM_CLOSE` to programs holding the files |
| `--keep-owners` | Never take ownership or change access rights |
| `-L, --find-locks` | Also list the processes holding each file (slower) |
| `--json` | One final machine-readable report on stdout, without banners or a second plan |
| `--plan-only` | Print the plan without deleting; with `--json`, emit one plan object |
| `--lang en\|ru` | Interface language |
| `-v` | More detail |

In JSON mode RAZE does not hand off to an elevated child: failures set
`needs_elevation` and return a nonzero exit code. Run the CLI elevated explicitly
when needed. Human-mode UAC handoff reports that a child was launched, not that
its deletion succeeded, and retries only failed targets.

### Language

English is the default when no preference has been saved. The **EN / RU**
button in the GUI title bar saves its choice immediately to
`HKCU\Software\RAZE` (`Language`) and restores it on the next launch. The CLI
uses the same saved default; `--lang` overrides it for that invocation without
changing the saved choice:

```powershell
.\raze.exe --lang ru
.\raze.exe --lang ru --help
```

## How deletion works

1. **Scan** — walk the tree *without entering reparse points*, classify every
   entry (file / folder / link), probe for blockers (reserved names, locks,
   ACLs) and check the Recycle Bin verdict for each top-level target.
2. **Plan** — decide the disposition per target *before* touching anything.
3. **Remove** — bottom-up, through handles: open with `\\?\`, apply
   `FILE_DISPOSITION_INFO_EX` (`DELETE | POSIX_SEMANTICS | IGNORE_READONLY_ATTRIBUTE`),
   fall back to the legacy disposition info where `Ex` is unavailable.
4. **Verify** — an object is only counted as removed when it is actually gone;
   `DELETE_PENDING` is retried, not assumed.
5. **Report** — per target: mode (Recycle Bin / permanent), counts, freed bytes,
   notes (ownership taken, locks closed) and every failure with its error code.

## Development

Windows x64, Rust 1.99.0 (pinned in `rust-toolchain.toml`) and Visual Studio Build
Tools (MSVC + Windows SDK 10.0.26100). Icon resources use the SDK's `rc.exe`:

```powershell
.\packaging\build-package.ps1
cargo build --locked --release --workspace
cargo test --locked --workspace --all-targets --all-features
```

`build-package.ps1` comes first: it packs and signs the sparse package that
gives `raze_shell.dll` an identity (needs `makeappx.exe` and `signtool.exe` from
the Windows SDK). The package is signed with a self-signed certificate whose
private key stays, non-exportable, in *your* user certificate store, so every
builder gets a certificate of their own: the script creates it on first run and
writes its thumbprint to `packaging\cert-thumbprint.txt`, which `cargo build`
compiles into `raze.exe` as the only certificate its installer will trust.
The committed value is the author's; yours replaces it in your working copy.

Artifacts in `target\release`:

| File | What |
|---|---|
| `raze.exe` | CLI |
| `raze-app.exe` | GUI |
| `raze_shell.dll` | COM shell extension for the modern Windows 11 menu |
| `RAZE.msix`, `RAZE.cer` | Signed sparse package and its public certificate (from `build-package.ps1`) |

To install the Explorer menu from a source build, run this **from the repository root**:

```powershell
.\target\release\raze.exe --install
```

The shell extension is a `cdylib` loaded into `explorer.exe`: it has no
`unwrap`/`expect` in shared paths, cannot panic, and `Invoke` only spawns
`raze.exe` detached — deletion never runs inside Explorer.

## Security

See [SECURITY.md](SECURITY.md) for the trust model, refusal list, and
shell-extension boundaries.

## License

[GPL-3.0-or-later](LICENSE).
