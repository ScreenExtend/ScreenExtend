# Manual test: the macOS app's code identity (and why TCC depends on it)

Nothing in this repository could sign anything, and nothing here signs anything now: the
release is signed by `tauri-action` with whatever identity the workflow's environment
carries. What was missing is any statement of **what that identity has to be**, and any
check that it stayed the same between releases. Both are written down here, and
`.github/workflows/build-release.yml` now enforces the second.

This is the app bundle. The *audio driver* package has its own procedure in
`macos-audio-driver-signing.md`; the two are separate identities and separate certificates.

## Why the identity is not cosmetic

TCC does not key a grant on an app's name or path. A row is keyed by
`(service, client bundle-id)` and carries a **`csreq`** blob — the code requirement captured
at the moment the user granted it (Apple TN3127, *Inside Code Signing: Requirements*).
`auth_value = 2` is what draws the checkmark in System Settings; the `csreq` match is what
actually authorizes the call. When the two disagree the user sees a ticked Screen Recording
box over a grant that does not apply, and `tccd` logs

```
Failed to match existing code requirement for subject app.screenextend.desktop
and service kTCCServiceScreenCapture
```

Three ways to land there:

1. **Ad-hoc signing.** `codesign --sign -` — which is what a plain `cargo build` or
   `pnpm tauri dev` produces — yields a designated requirement pinned to the exact
   **cdhash**, and the cdhash changes on every rebuild. Apple DTS's guidance is to use a
   stable identity (Apple Development locally, Developer ID for distribution) and **never
   mix ad-hoc and Developer ID on one bundle identifier**.
2. **macOS 15 makes it fatal rather than annoying.** `CGPreflightScreenCaptureAccess()` and
   ScreenCaptureKit are reported to require a binary signed with a Developer ID certificate
   carrying a Team ID; an ad-hoc identity reads `false` no matter what the user granted.
3. **App Translocation.** A quarantined bundle opened through Launch Services and never
   moved *by Finder* runs from a read-only
   `/private/var/folders/.../AppTranslocation/<uuid>/d/ScreenExtend.app`. The updater then
   writes to a read-only volume, and the identity in the Screen Recording list is not the
   path that is running. `doctor` already reports this (`macos.translocated`).

## The requirement

- Every public artifact is signed **Developer ID Application**, hardened runtime on, with a
  Team ID, and notarized.
- The bundle identifier `app.screenextend.desktop` is **only ever** used with that identity.
  A locally signed experiment must use a different identifier, or it poisons the release
  build's TCC rows on that machine.
- The designated requirement string must be **identical across consecutive releases**.

## Seeding the checked-in expectation (once)

The release job compares the built app's designated requirement against
`src-tauri/resources/EXPECTED_DESIGNATED_REQUIREMENT.txt`. That file is not in the tree
yet, because no one has a signed build to read it from. Until it exists the job prints the
requirement and emits a warning instead of failing. To seed it, on a Mac with a signed
`ScreenExtend.app`:

```sh
codesign -d -r- ScreenExtend.app 2>&1 | sed -n 's/^designated => //p' \
  > src-tauri/resources/EXPECTED_DESIGNATED_REQUIREMENT.txt
cat src-tauri/resources/EXPECTED_DESIGNATED_REQUIREMENT.txt
```

It should read roughly

```
identifier "app.screenextend.desktop" and anchor apple generic and
certificate leaf[subject.OU] = "<TEAMID>"
```

Commit it. From then on a release whose identity has drifted fails the job with a diff,
before `finalize-release` publishes anything.

## Checking a release by hand (5 min)

```sh
app=ScreenExtend.app
codesign -dv --verbose=4 "$app"        # Authority=Developer ID Application: ...
                                       # TeamIdentifier=<TEAMID>   (never "not set")
                                       # Signature != adhoc
codesign -d -r- "$app"                 # the designated requirement
spctl -a -vvv -t install "$app"        # accepted, source=Notarized Developer ID
xcrun stapler validate "$app"          # The validate action worked!
```

## Proving a grant survives an update (30 min, needs a clean VM)

1. Install version *n*, launch it, grant Screen Recording, confirm a device can join.
2. Install version *n+1* over it.
3. Launch. The grant must still apply: no re-prompt, and
   `log stream --predicate 'subsystem == "com.apple.TCC"'` must show **no**
   `Failed to match existing code requirement` line for `app.screenextend.desktop`.
4. `ScreenExtend doctor --json | jq '.macos.designatedRequirement, .macos.translocated'`
   should show the same requirement as step 1 and `false`.

## When a user's grant has gone stale

```sh
tccutil reset ScreenCapture app.screenextend.desktop
tccutil reset Accessibility app.screenextend.desktop
```

then relaunch and grant again. `doctor` prints both commands for exactly this case.

## Translocation

Copying the app out of the DMG with Finder disables translocation permanently; `mv` from a
terminal does not. If `doctor` reports `translocated: true`, the fix is to move
`ScreenExtend.app` to `/Applications` **in Finder** and relaunch.
