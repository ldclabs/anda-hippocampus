package cmd

import (
	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

var recallCmd = &cobra.Command{
	Use:   "recall <query>",
	Short: "Recall memory via natural-language query",
	Long: `Query memory with natural language. The query should describe
what information you want to retrieve from memory.

Example:
  anda-cli recall "What are the user's preferences?"
  anda-cli recall --context-counterparty u1 "What happened in the last meeting?"`,
	Args: cobra.ExactArgs(1),
	Run: func(cmd *cobra.Command, args []string) {
		contextCounterparty, _ := cmd.Flags().GetString("context-counterparty")
		if contextCounterparty == "" {
			// Fall back to the deprecated alias.
			contextCounterparty, _ = cmd.Flags().GetString("context-user")
		}
		contextAgent, _ := cmd.Flags().GetString("context-agent")
		contextTopic, _ := cmd.Flags().GetString("context-topic")

		input := &api.RecallInput{
			Query: args[0],
		}

		ctx := buildInputContext(contextCounterparty, contextAgent, "", contextTopic)
		if ctx != nil {
			input.Context = ctx
		}

		client := newClient()
		resp, err := client.Recall(cmd.Context(), input)
		if err != nil {
			exitError(err)
		}
		if resp.Error != nil {
			exitError(resp.Error)
		}
		if resp.Result != nil {
			printJSON(resp.Result)
		}
	},
}

func init() {
	recallCmd.Flags().String("context-counterparty", "", "Context counterparty (e.g. user ID)")
	recallCmd.Flags().String("context-user", "", "Context counterparty (deprecated alias)")
	_ = recallCmd.Flags().MarkDeprecated("context-user", "use --context-counterparty instead")
	recallCmd.Flags().String("context-agent", "", "Context agent")
	recallCmd.Flags().String("context-topic", "", "Context topic")
	rootCmd.AddCommand(recallCmd)
}
