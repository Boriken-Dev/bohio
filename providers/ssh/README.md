# bohio-provider-ssh

A provider for [Bohio](https://github.com/Boriken-Dev/bohio): a machine reached over SSH, with nothing of Bohio installed on it.
Its terminal, its processes and (with SFTP) its files are served to a node's peers through the same grants as the node's own.
Configure it as `[source.NAME] provider = "ssh"` with `host`; `AGENTS.md` lists every key.

Licence MPL-2.0.
