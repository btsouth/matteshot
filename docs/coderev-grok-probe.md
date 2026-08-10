# CodeRev Grok probe

Scratch file for a one-off CI probe: CodeRev's Grok generator has been
returning zero candidates on every run while the second generator carried the
whole review. The wrapper flags were corrected and verified by hand, but the
failure persisted in CI, so the pipeline now logs the raw completion whenever
it parses to no findings.

This PR exists to trigger one review and read that log line. Delete the file
once the question is answered.
