-- Add via notification preferences
ALTER TABLE notification_preferences
ADD COLUMN via_likes BOOLEAN NOT NULL DEFAULT TRUE,
ADD COLUMN via_reposts BOOLEAN NOT NULL DEFAULT TRUE;