# db

SQLite schemaと共有query。gateway固有のidentity、lifecycle、projection、transplantは所有しない。

Session ID とgate instanceの`kind_id`はopaque値として保存し、共有queryでは意味を解釈しない。gateway のセッションキーは `gate_bindings.session_key`（v65）で core のセッションに対応付ける（D-1099）。
