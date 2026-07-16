package cmd

import (
	"fmt"
	"os"
	"strings"

	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

var managementCmd = &cobra.Command{
	Use:   "management",
	Short: "Space management operations (requires CWT auth)",
}

var listTokensCmd = &cobra.Command{
	Use:   "list-tokens",
	Short: "List space tokens",
	Run: func(cmd *cobra.Command, args []string) {
		client := newClient()
		resp, err := client.ListSpaceTokens(cmd.Context())
		if err != nil {
			exitError(err)
		}
		if resp.Error != nil {
			exitError(resp.Error)
		}
		printJSON(resp.Result)
	},
}

var addTokenCmd = &cobra.Command{
	Use:   "add-token",
	Short: "Add a space token",
	Run: func(cmd *cobra.Command, args []string) {
		scope, _ := cmd.Flags().GetString("scope")
		name, _ := cmd.Flags().GetString("name")
		labels, _ := cmd.Flags().GetStringSlice("labels")
		if scope != "read" && scope != "write" && scope != "*" {
			exitError(fmt.Errorf("invalid scope: %s", scope))
		}
		if name == "" {
			exitError(fmt.Errorf("--name is required"))
		}
		if len(labels) > 0 && scope != "read" {
			exitError(fmt.Errorf("--labels requires --scope read: label-restricted tokens are read-only wiki viewers"))
		}

		input := &api.AddSpaceTokenInput{
			Scope: api.TokenScope(scope),
			Name:  name,
		}
		if len(labels) > 0 {
			input.Labels = labels
		}

		client := newClient()
		resp, err := client.AddSpaceToken(cmd.Context(), input)
		if err != nil {
			exitError(err)
		}
		if resp.Error != nil {
			exitError(resp.Error)
		}
		printJSON(resp.Result)
	},
}

var revokeTokenCmd = &cobra.Command{
	Use:   "revoke-token [token]",
	Short: "Revoke a space token by full value, or by --name",
	Long: `Revoke a space token by its full value (positional argument), or by its
unique name via --name. list-tokens only echoes a display prefix of each
token, so --name is the way to revoke a token whose full value was not
saved at mint time. Provide exactly one of the two.`,
	Args: cobra.MaximumNArgs(1),
	Run: func(cmd *cobra.Command, args []string) {
		name, _ := cmd.Flags().GetString("name")
		hasToken := len(args) == 1 && args[0] != ""
		hasName := name != ""
		if hasToken == hasName {
			exitError(fmt.Errorf("provide exactly one of <token> or --name"))
		}

		client := newClient()
		var resp *api.RpcResponse[bool]
		var err error
		if hasToken {
			resp, err = client.RevokeSpaceToken(cmd.Context(), args[0])
		} else {
			resp, err = client.RevokeSpaceTokenByName(cmd.Context(), name)
		}
		if err != nil {
			exitError(err)
		}
		if resp.Error != nil {
			exitError(resp.Error)
		}
		printJSON(resp.Result)
	},
}

var updateSpaceCmd = &cobra.Command{
	Use:   "update-space",
	Short: "Update space information",
	Run: func(cmd *cobra.Command, args []string) {
		input := &api.UpdateSpaceInput{}
		hasField := false

		if cmd.Flags().Changed("name") {
			v, _ := cmd.Flags().GetString("name")
			input.Name = &v
			hasField = true
		}
		if cmd.Flags().Changed("description") {
			v, _ := cmd.Flags().GetString("description")
			input.Description = &v
			hasField = true
		}
		if cmd.Flags().Changed("public") {
			v, _ := cmd.Flags().GetBool("public")
			input.Public = &v
			hasField = true
		}
		if cmd.Flags().Changed("wiki-digest") {
			v, _ := cmd.Flags().GetBool("wiki-digest")
			input.WikiDigest = &v
			hasField = true
		}
		if cmd.Flags().Changed("wiki-audit-reads") {
			v, _ := cmd.Flags().GetBool("wiki-audit-reads")
			input.WikiAuditReads = &v
			hasField = true
		}
		if cmd.Flags().Changed("wiki-acl-defaults") {
			pairs, _ := cmd.Flags().GetStringSlice("wiki-acl-defaults")
			defaults, err := parseACLDefaults(pairs)
			if err != nil {
				exitError(err)
			}
			input.WikiACLDefaults = &defaults
			hasField = true
		}

		if !hasField {
			exitError(fmt.Errorf("at least one of --name, --description, --public, --wiki-digest, --wiki-audit-reads, or --wiki-acl-defaults is required"))
		}

		client := newClient()
		resp, err := client.UpdateSpace(cmd.Context(), input)
		if err != nil {
			exitError(err)
		}
		if resp.Error != nil {
			exitError(resp.Error)
		}
		fmt.Println("Space updated successfully")
	},
}

var restartFormationCmd = &cobra.Command{
	Use:   "restart-formation",
	Short: "Restart a formation task (manager only)",
	Run: func(cmd *cobra.Command, args []string) {
		input := &api.RestartFormationInput{}

		v, _ := cmd.Flags().GetUint64("conversation")
		if v == 0 {
			exitError(fmt.Errorf("--conversation is required"))
		}

		input.Conversation = &v
		client := newClient()
		resp, err := client.RestartFormation(cmd.Context(), input)
		if err != nil {
			exitError(err)
		}
		if resp.Error != nil {
			exitError(resp.Error)
		}
		fmt.Println("Formation restarted successfully")
	},
}

var getBYOKCmd = &cobra.Command{
	Use:   "get-byok",
	Short: "Get BYOK (Bring Your Own Key) configuration (manager only)",
	Run: func(cmd *cobra.Command, args []string) {
		client := newClient()
		resp, err := client.GetBYOK(cmd.Context())
		if err != nil {
			exitError(err)
		}
		if resp.Error != nil {
			exitError(resp.Error)
		}
		printJSON(resp.Result)
	},
}

var updateBYOKCmd = &cobra.Command{
	Use:   "update-byok",
	Short: "Update BYOK (Bring Your Own Key) configuration (manager only)",
	Run: func(cmd *cobra.Command, args []string) {
		input := &api.ModelConfig{}

		family, _ := cmd.Flags().GetString("family")
		model, _ := cmd.Flags().GetString("model")
		apiBase, _ := cmd.Flags().GetString("api-base")
		apiKeyInput, _ := cmd.Flags().GetString("api-key")

		apiKey, err := resolveSecretInput(apiKeyInput)
		if err != nil {
			exitError(fmt.Errorf("resolve --api-key: %w", err))
		}

		if family == "" || model == "" || apiBase == "" || apiKey == "" {
			exitError(fmt.Errorf("--family, --model, --api-base, and --api-key (or ANDA_BYOK_API_KEY) are required"))
		}

		input.Family = family
		input.Model = model
		input.APIBase = apiBase
		input.APIKey = apiKey
		if cmd.Flags().Changed("disabled") {
			disabled, _ := cmd.Flags().GetBool("disabled")
			input.Disabled = &disabled
		}
		input.Label, _ = cmd.Flags().GetString("label")
		input.BearerAuth, _ = cmd.Flags().GetBool("bearer-auth")
		input.Stream, _ = cmd.Flags().GetBool("stream")
		input.ContextWindow, _ = cmd.Flags().GetInt("context-window")
		input.MaxOutput, _ = cmd.Flags().GetInt("max-output")

		client := newClient()
		resp, err := client.UpdateBYOK(cmd.Context(), input)
		if err != nil {
			exitError(err)
		}
		if resp.Error != nil {
			exitError(resp.Error)
		}
		fmt.Println("BYOK configuration updated successfully")
	},
}

// parseACLDefaults parses "namespace=label" pairs into a map. An empty pairs
// list yields an empty map, which the server treats as "clear all defaults".
func parseACLDefaults(pairs []string) (map[string]string, error) {
	defaults := make(map[string]string, len(pairs))
	for _, pair := range pairs {
		ns, label, ok := strings.Cut(pair, "=")
		ns = strings.TrimSpace(ns)
		label = strings.TrimSpace(label)
		if !ok || ns == "" || label == "" {
			return nil, fmt.Errorf("invalid --wiki-acl-defaults entry %q: expected namespace=label", pair)
		}
		defaults[ns] = label
	}
	return defaults, nil
}

func init() {
	addTokenCmd.Flags().String("name", "", "Token name (required)")
	addTokenCmd.Flags().String("scope", "*", "Token scope: read, write, *")
	addTokenCmd.Flags().StringSlice("labels", nil, "Wiki ACL labels the token may read, comma-separated or repeated (requires --scope read; unrestricted when omitted)")
	restartFormationCmd.Flags().Uint64("conversation", 0, "Conversation ID")

	revokeTokenCmd.Flags().String("name", "", "Revoke by unique token name instead of full token value")

	updateSpaceCmd.Flags().String("name", "", "Space name")
	updateSpaceCmd.Flags().String("description", "", "Space description")
	updateSpaceCmd.Flags().Bool("public", false, "Whether space is public")
	updateSpaceCmd.Flags().Bool("wiki-digest", false, "Enable/disable WikiDigest background extraction (--wiki-digest=false to disable)")
	updateSpaceCmd.Flags().Bool("wiki-audit-reads", false, "Enable/disable read auditing for external wiki reads (--wiki-audit-reads=false to disable)")
	updateSpaceCmd.Flags().StringSlice("wiki-acl-defaults", nil, "Namespace default ACL labels as namespace=label pairs, comma-separated or repeated; replaces the whole map (pass \"\" to clear all defaults)")

	updateBYOKCmd.Flags().String("family", "", "Model family (e.g. gemini, anthropic, openai, deepseek, mimo) (required)")
	updateBYOKCmd.Flags().String("model", "", "Model name (required)")
	updateBYOKCmd.Flags().String("api-base", "", "Model API base URL (required)")
	updateBYOKCmd.Flags().String("api-key", os.Getenv("ANDA_BYOK_API_KEY"), "Model API key, or @file/path to a file containing it (required; env: ANDA_BYOK_API_KEY)")
	updateBYOKCmd.Flags().Bool("disabled", false, "Whether the BYOK config is disabled")
	updateBYOKCmd.Flags().String("label", "", "Model label")
	updateBYOKCmd.Flags().Bool("bearer-auth", false, "Send the API key as a Bearer Authorization header")
	updateBYOKCmd.Flags().Bool("stream", false, "Enable streaming responses from the model provider")
	updateBYOKCmd.Flags().Int("context-window", 0, "Model context window in tokens (0 = provider default)")
	updateBYOKCmd.Flags().Int("max-output", 0, "Model max output tokens (0 = provider default)")

	managementCmd.AddCommand(listTokensCmd)
	managementCmd.AddCommand(addTokenCmd)
	managementCmd.AddCommand(revokeTokenCmd)
	managementCmd.AddCommand(updateSpaceCmd)
	managementCmd.AddCommand(restartFormationCmd)
	managementCmd.AddCommand(updateBYOKCmd)
	managementCmd.AddCommand(getBYOKCmd)
	rootCmd.AddCommand(managementCmd)
}
