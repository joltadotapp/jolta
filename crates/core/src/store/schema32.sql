-- The video calls a calendar's server lets it make, as the calendar list
-- says: Graph's allowedOnlineMeetingProviders, Google's
-- conferenceProperties.allowedConferenceSolutionTypes, as Jolta's
-- provider names, comma-separated. NULL until a list says, and for
-- sources that never say.
ALTER TABLE calendar ADD COLUMN video TEXT;
