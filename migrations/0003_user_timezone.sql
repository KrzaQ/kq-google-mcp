-- The zone a person's times are shown in, and the zone a time without an
-- offset is read on. Every instant stays UTC in the database; this column
-- only decides how it is rendered and how a naive argument is understood.
--
-- The default matches the house zone GMCP_TIMEZONE ships with, so rows made
-- before this migration keep working and the column can be NOT NULL. Adding a
-- column leaves the table STRICT.
ALTER TABLE users ADD COLUMN timezone TEXT NOT NULL DEFAULT 'Europe/Warsaw';
