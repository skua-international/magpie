module.exports = {
  branches: ["main"],
  plugins: [
    "@semantic-release/commit-analyzer",
    "@semantic-release/release-notes-generator",
    [
      "@semantic-release/github",
      {
        // The App token only has Contents: Read & Write -- successComment
        // (commenting on issues/PRs referenced by a release's commits)
        // needs issues/PR write access it doesn't have, which threw
        // "Resource not accessible by integration" in the plugin's
        // "success" step *after* the tag/release had already been
        // created successfully. That made the whole job report failure
        // anyway, which meant build-images.yml's workflow_run gate (only
        // triggers on conclusion == 'success') never fired -- a real
        // release existed with no images ever built for it. This
        // behavior is optional to the release itself; disable both
        // rather than grant the App broader permissions it doesn't
        // otherwise need.
        successComment: false,
        failComment: false,
      },
    ],
  ],
};
