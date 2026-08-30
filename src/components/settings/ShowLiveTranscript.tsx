import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface ShowLiveTranscriptProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

/**
 * Whether the recording overlay shows the still-changing transcript while you
 * speak. Display only — the streaming backend runs either way, so turning this
 * off costs no speed.
 */
export const ShowLiveTranscript: React.FC<ShowLiveTranscriptProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const enabled = getSetting("show_live_transcript") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(value) => updateSetting("show_live_transcript", value)}
        isUpdating={isUpdating("show_live_transcript")}
        label={t("settings.general.showLiveTranscript.label")}
        description={t("settings.general.showLiveTranscript.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);

ShowLiveTranscript.displayName = "ShowLiveTranscript";
