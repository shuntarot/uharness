# targets-private

Where target descriptions that are not published live: your own boards,
prototypes, anything with no public board file to point at.

- **Equal to `targets/`.** Same namespace, same schema, reached the same way with
  `--target <provider>/<board>`. Splitting them into "built in = first class" and
  "user defined = unofficial" would make the boards you actually use the
  unofficial ones.
- **The contents are gitignored**: only this README is tracked (the rule is
  in the root `.gitignore`). Descriptions live on the machines that build for
  those boards, and never enter the history of a repository that is going
  public. They are still embedded in a binary built from such a checkout, so do
  not publish one.
- **This directory can be a git repository of its own**, kept somewhere
  private: clone it here, or `git init` here and add a remote. Its own
  `.gitignore` should list `README.md`, which belongs to the outer
  repository.
- **They show up in `veryl harness targets`**, marked `(private)`, on the
  machines that have them. The all-targets tests run them there too.
- **A name present in both `targets/` and here is an error.** There is no
  precedence. To change part of a public board, use
  `--target-patch`.

An empty directory builds fine — there is simply nothing to embed.
