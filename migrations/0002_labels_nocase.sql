-- A connection is named by its label, and every lookup folds case: a tool's
-- `account` argument resolves through `lower(label) = lower(?)`. The UNIQUE
-- (user_id, label) of 0001 compares bytes, so "work" and "Work" were two rows
-- that both answered to "work" and whichever came back first won.
--
-- A unique index with the NOCASE collation on the label makes the pair
-- conflict, which is what the lookups already assume. The byte-wise UNIQUE
-- from 0001 stays where it is; it can only fire where this one already has.
CREATE UNIQUE INDEX connections_user_label_nocase
    ON connections (user_id, label COLLATE NOCASE);
