package actions

import (
	"context"
	"fmt"
	"os/exec"
	"strings"
)

// BaselineConfigMapName mirrors the chart's own naming
// (magpie.fullname-arma-config-baseline, see charts/magpie/templates/
// arma-config-baseline-configmap.yaml) -- fullname is just the Helm
// release name in this chart, so this needs no live cluster lookup.
// Shared by cmd/armaconfig.go (direct CLI) and the TUI's Admin screen
// (see tui/admin_actions.go) so both compute the same name from the
// same release string instead of duplicating the convention.
func BaselineConfigMapName(release string) string {
	return release + "-arma-config-baseline"
}

// ArmaConfigFieldGuide is the per-key reference for every key
// services/controller/src/arma_config.rs reads, set as an annotation on
// a freshly-created per-server override ConfigMap so it's visible
// directly in `kubectl edit`'s buffer -- ConfigMap `data` itself can't
// carry comments (they don't survive the API round-trip `kubectl edit`
// does: it fetches the live object fresh, which was never stored with
// any). Keep in sync with charts/magpie/templates/_helpers.tpl's
// magpie.armaConfigFieldGuide (the baseline ConfigMap's own copy,
// chart-templated -- this one can't reference that at all, since
// per-server override ConfigMaps are created via plain kubectl, not
// Helm) and README.md's "Arma server config" table.
const ArmaConfigFieldGuide = `main.cfg keys (all string values; bools/numbers as their literal string form):
  hostname            placeholders; unset -> "{{prefix}}{{server_name}}{{suffix}}"
  prefix / suffix      hostname placeholders only, no direct field
  max_players           -> maxPlayers (default 64)
  force_difficulty / forced_difficulty  -> forcedDifficulty (omitted unless forced)
  missions_whitelist    comma-separated -> missionWhitelist[]
  persist_without_players -> persistent
  use_battleEye          -> BattlEye
  verify_signatures      -> verifySignatures (2/0, default true)
  skip_lobby             -> skipLobby
  allowed_file_patching  0/1/2 -> allowedFilePatching (default 1)
  disable_von            -> disableVON (default true)
  kick_timeout          "level:seconds,..." -> kickTimeout[] (default 0:1,1:1,2:1,3:1)
  allow_zeus_composition_scripts -> zeusCompositionScriptLevel (2/0)
  allow_custom_glasses   -> allowProfileGlasses
  max_ping / max_packet_loss / max_desync  numbers, unset = omitted (max_ping default 300)
  password_admin / password / server_command_password  placeholders + {{secret:name/key}}
  motd                  comma-separated, placeholders -> motd[]
  motd_interval          number, unset = omitted
  other_properties       raw text, appended verbatim at the end
  admins[]/filePatchingExceptions[] are never keys here -- computed from
  arma:admin/arma:filepatch scope grants every reconcile.
basic.cfg keys (all default unset/omitted): max_msg_send, max_size_guaranteed,
  max_size_nonguaranteed, min_bandwidth, max_bandwidth, min_error_to_send,
  min_error_to_send_near, basic_other_properties.
launch flag keys (not main.cfg/basic.cfg content -- these become the
  launcher's own -limitFPS=/extra argv, same merged ConfigMap regardless):
  limit_fps               -> -limitFPS= (default 300)
  additional_params        extra launch args, appended verbatim after
                            the generated -mod=/CDLC ones (replaces the
                            old per-server spec.params field -- this is
                            the one way to set them now)
env.<NAME> keys become extra launcher container env vars (same
  placeholder/secret support). Full docs: README.md "Arma server config".`

// EnsureConfigMapExists creates an empty ConfigMap (annotated with
// ArmaConfigFieldGuide) if one by this name doesn't already exist --
// `kubectl edit` (unlike `kubectl apply`) fails outright against a
// nonexistent object, and neither a fresh per-server config override
// nor (in principle) the chart-managed baseline are guaranteed to
// already be there. Uses CombinedOutput rather than streaming to
// os.Stderr directly: this runs the same way under the TUI (mid-render,
// alt-screen active) as it does under the plain CLI, and a stray direct
// terminal write from the TUI's side would corrupt the display.
func EnsureConfigMapExists(ctx context.Context, namespace, name string) error {
	if err := exec.CommandContext(ctx, "kubectl", "get", "configmap", name, "-n", namespace).Run(); err == nil {
		return nil
	}
	out, err := exec.CommandContext(ctx, "kubectl", "create", "configmap", name, "-n", namespace).CombinedOutput()
	if err != nil {
		return fmt.Errorf("kubectl create configmap %s: %w: %s", name, err, strings.TrimSpace(string(out)))
	}
	annotateArgs := []string{
		"annotate", "configmap", name, "-n", namespace,
		"magpie.skua.io/field-guide=" + ArmaConfigFieldGuide,
	}
	if out, err := exec.CommandContext(ctx, "kubectl", annotateArgs...).CombinedOutput(); err != nil {
		return fmt.Errorf("kubectl annotate configmap %s: %w: %s", name, err, strings.TrimSpace(string(out)))
	}
	return nil
}

// ConfigMapEditCmd builds (but doesn't run) the `kubectl edit` command for
// a ConfigMap, reusing kubectl's own $EDITOR handling and get/apply-on-
// save flow rather than hand-rolling a temp-file/diff/apply cycle.
// Returned unstarted with Stdin/Stdout/Stderr left nil -- the direct CLI
// wires those to os.Stdin/Stdout/Stderr itself before Run(), while the
// TUI hands this straight to tea.ExecProcess, which fills them in around
// suspending/resuming the terminal instead.
func ConfigMapEditCmd(ctx context.Context, namespace, name string) *exec.Cmd {
	return exec.CommandContext(ctx, "kubectl", "edit", "configmap", name, "-n", namespace)
}
