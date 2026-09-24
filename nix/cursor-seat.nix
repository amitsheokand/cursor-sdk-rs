# Build cursor-seat (workspace member) with nixpkgs rustPlatform.
# OSS-safe: no private hostnames, product names, or user paths.
{
  lib,
  rustPlatform,
  makeWrapper,
  cursor-sdk-bridge,
}:

rustPlatform.buildRustPackage {
  pname = "cursor-seat";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = lib.cleanSource ../.;
    filter =
      path: _type:
      let
        name = baseNameOf path;
      in
      name != "target"
      && name != ".git"
      && name != ".DS_Store"
      && name != "result"
      && !(lib.hasPrefix "result-" name)
      && !(lib.hasSuffix ".md" name && lib.hasPrefix "RECEIPT-" name)
      && !(lib.hasSuffix ".md" name && lib.hasPrefix "PACKET" name);
  };

  cargoLock.lockFile = ../Cargo.lock;

  # When cursor-seat adds a git dependency on toolgate, add cargoLock.outputHashes.
  cargoBuildFlags = [
    "-p"
    "cursor-seat"
  ];

  nativeBuildInputs = [
    makeWrapper
  ];

  # prost-build/protox in build.rs — no protoc required.
  doCheck = true;

  cargoTestFlags = [
    "-p"
    "cursor-seat"
  ];

  # Skips: Nix check sandbox has no `git` on PATH and cannot mkdir under $HOME.
  checkFlags = [
    "--skip"
    "attribute_agent_commit_in_worktree_then_copy_is_escape" # needs git init/config in temp repo
    "--skip"
    "attribute_escape_when_protected_root_is_subdirectory" # needs git init/config in temp repo
    "--skip"
    "attribute_literal_pathspec_with_glob_chars" # needs git init/config in temp repo
    "--skip"
    "attribute_new_untracked_worktree_copy_is_escape" # needs git init/config in temp repo
    "--skip"
    "attribute_owner_commit_same_bytes_in_primary_is_external" # needs git init/config in temp repo
    "--skip"
    "attribute_owner_edit_in_subdirectory_repo_is_external" # needs git init/config in temp repo
    "--skip"
    "attribute_primary_revert_to_head_while_worktree_clean_is_external" # needs git init/config in temp repo
    "--skip"
    "attribute_rename_in_primary_is_external" # needs git init/config in temp repo
    "--skip"
    "attribute_worktree_copy_is_escape" # needs git init/config in temp repo
    "--skip"
    "drift_path_with_space" # needs git commit in temp repo
    "--skip"
    "fence_only_outside_entries_yields_no_drift" # needs git init/config in temp repo
    "--skip"
    "shell_command_home_spelling_variants" # writes under $HOME (sandbox blocks)
    "--skip"
    "tilde_fence_entry_under_cwd_matches" # writes under $HOME (sandbox blocks)
    "--skip"
    "attach_protected_baseline_only_reports_post_attach_changes" # fence_run: git init/config
    "--skip"
    "attach_resume_drift_correction_then_fence_drift" # fence_run: git init/config
    "--skip"
    "in_fence_git_change_stays_ok" # fence_run: git init/config
    "--skip"
    "out_of_fence_git_drift_gets_one_correction_then_fails" # fence_run: git init/config
    "--skip"
    "protected_root_change_bounces" # fence_run: git init/config
    "--skip"
    "protected_root_external_then_agent_copy_escapes" # fence_run: git init/config
    "--skip"
    "protected_root_git_failure_post_drive_emits_undecided" # fence_run: git init/config
    "--skip"
    "protected_root_midrun_escape_after_tool_call" # fence_run: git init/config
    "--skip"
    "protected_root_midrun_external_edit_notices_once" # fence_run: git init/config
    "--skip"
    "protected_root_post_drive_external_only" # fence_run: git init/config
    "--skip"
    "tool_call_completion_runs_protected_snapshot_when_due" # fence_run: git init/config
  ];

  postInstall = ''
    wrapProgram $out/bin/cursor-seat \
      --set-default CURSOR_SDK_BRIDGE_BIN ${cursor-sdk-bridge}/bin/cursor-sdk-bridge
  '';

  meta = with lib; {
    description = "Cursor seat binary: typed stdin inbox and durable agent runs";
    license = licenses.mit;
    mainProgram = "cursor-seat";
    platforms = platforms.unix;
  };
}
