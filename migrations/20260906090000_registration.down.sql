-- 登録・認証トラックのロールバック（開発環境で up を検証・やり直すためのもの）。
--
-- player_declarations は自己申告の追記ログであり、データが入った環境でこれを実行すると
-- レート seed の入力が失われる。運用環境で「取り消し」の手段として使ってはならない。
-- トリガーとシーケンスはテーブルと共に削除される。
drop table player_declarations;

alter table players drop column registered_at;
alter table players drop column status;
alter table players drop column trainer_name;
