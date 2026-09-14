# db

SQLite schemaと共有query。gateway固有のidentity、lifecycle、projection、transplantは所有しない。

Session ID、gate instanceの`kind_id`、trusted userのsourceはopaque値として保存し、共有queryでは意味を解釈しない。Gateway bindingの作成は汎用`create_gate_binding_in_tx`を唯一のtransaction部品として使う。
