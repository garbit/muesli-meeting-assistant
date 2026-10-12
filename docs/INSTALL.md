# Installing Muesli

## Requirements

* Apple Silicon Mac (M1 or newer) running macOS 14.4 or later.
* 16 GB RAM for the Fast preset, 24 GB or more for Balanced.
* About 1 GB for the speech models and 8 to 15 GB for the summarisation model.

## First install

1. Download `Muesli_x.y.z_aarch64.dmg` from the GitHub release and drag Muesli to
   Applications.
2. Muesli is not notarised yet. On first launch macOS will refuse to open it.
   Right-click (or Control-click) Muesli in Applications and choose **Open**,
   then **Open** again in the dialog. If macOS says the app is "damaged", run:

   ```sh
   xattr -d com.apple.quarantine /Applications/Muesli.app
   ```

3. Open **Models** and install the **Balanced** preset (or **Fast** on a 16 GB
   Mac). Downloads come from Hugging Face and resume if interrupted.
4. On the first launch Muesli shows a short welcome card and macOS asks for
   two permissions, both attributed to Muesli:
   * **Microphone**: your side of the call.
   * **System Audio Recording**: the other participants. This appears under
     *Privacy & Security → Screen & System Audio Recording*.

   Allow both and the card closes by itself. If you refuse one, the card (and
   later a banner) says which one and offers **Open System Settings**; Muesli
   re-checks when you come back to its window, or on **Re-check** under
   **Settings → Permissions**.

## After an update

**Settings → Check for updates** downloads the new version from the GitHub release,
verifies its signature and relaunches Muesli.

Muesli is signed ad-hoc, so macOS treats every build as a new application and
forgets the two permissions above. On the first launch of a new build the
prompts run again automatically. If macOS reports a permission as off even
though System Settings shows Muesli as allowed, the entry belongs to the
previous build: turn it off and on again (or remove it with the − button and
re-grant). This goes away once releases are signed with a Developer ID.

## Where data lives

Everything is under `~/Library/Application Support/Muesli/`:

* `muesli.db` — meetings, transcripts, summaries, templates, known voices
* `meetings/<id>/` — `recording.m4a` (AAC, left = mic, right = system),
  `asr.json`, `diar.json`; raw `chunk-*.pcm` files appear only while recording
  or after a crash (Muesli rebuilds the recording from them on the next launch)
* `models/` — downloaded models

Nothing is sent anywhere. Delete the folder to remove all data.
