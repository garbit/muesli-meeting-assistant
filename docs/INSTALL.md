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
4. Press **Record** once with nothing playing. macOS asks for two permissions,
   both attributed to Muesli:
   * **Microphone**: your side of the call.
   * **System Audio Recording**: the other participants. This appears under
     *Privacy & Security → Screen & System Audio Recording*.

   Stop the test recording; it can be deleted from the library.

## After an update

Muesli is signed ad-hoc, so macOS treats every build as a new application and
forgets the two permissions above. After an update, open **Settings →
Permissions** and re-grant them. This goes away once releases are signed with a
Developer ID.

## Where data lives

Everything is under `~/Library/Application Support/Muesli/`:

* `muesli.db` — meetings, transcripts, summaries, templates, known voices
* `meetings/<id>/` — `recording.m4a` (AAC, left = mic, right = system),
  `asr.json`, `diar.json`; raw `chunk-*.pcm` files appear only while recording
  or after a crash (Muesli rebuilds the recording from them on the next launch)
* `models/` — downloaded models

Nothing is sent anywhere. Delete the folder to remove all data.
