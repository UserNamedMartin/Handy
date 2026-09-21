import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { GeminiTranscribeMode } from "@/bindings";
import { useSettings } from "../../../hooks/useSettings";
import { Input } from "../../ui/Input";
import { ToggleSwitch } from "../../ui/ToggleSwitch";

const GEMINI_PROVIDER = "gemini";
/** Catalog ids served by the Gemini backend. */
const GEMINI_MODELS = ["gemini-3.5-transcribe", "gemini-3.5-transcribe-live"];

const ELEVENLABS_PROVIDER = "elevenlabs";
/** Catalog ids served by the ElevenLabs backend. */
const ELEVENLABS_MODELS = ["elevenlabs-scribe-v2"];

export const isGeminiModel = (modelId: string | undefined): boolean =>
  !!modelId && GEMINI_MODELS.includes(modelId);

export const isElevenLabsModel = (modelId: string | undefined): boolean =>
  !!modelId && ELEVENLABS_MODELS.includes(modelId);

/** Any model whose audio leaves this machine, i.e. one this card configures. */
export const isCloudModel = (modelId: string | undefined): boolean =>
  isGeminiModel(modelId) || isElevenLabsModel(modelId);

/** Comma-separated text ⇄ list, tolerant of stray whitespace and empties. */
const parseList = (raw: string): string[] =>
  raw
    .split(",")
    .map((item) => item.trim())
    .filter(Boolean);

const Row: React.FC<{
  label: string;
  hint?: string;
  children: React.ReactNode;
}> = ({ label, hint, children }) => (
  <div className="flex flex-col gap-1 px-3 py-2.5 border-t border-mid-gray/20 first:border-t-0">
    <div className="flex items-center justify-between gap-3">
      <span className="text-sm text-text">{label}</span>
      {children}
    </div>
    {hint && <span className="text-[0.7rem] text-text/40">{hint}</span>}
  </div>
);

/**
 * The one row every cloud provider needs. Keyed by provider so the stored map
 * can hold a key per backend and switching models does not clobber the other.
 */
const ApiKeyRow: React.FC<{
  provider: string;
  placeholder: string;
  hint: string;
}> = ({ provider, placeholder, hint }) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting } = useSettings();
  const keys = (getSetting("cloud_api_keys") ?? {}) as Record<string, string>;
  const storedKey = keys[provider] ?? "";
  const [apiKey, setApiKey] = useState(storedKey);

  useEffect(() => setApiKey(storedKey), [storedKey]);

  return (
    <Row label={t("settings.models.cloud.apiKey")} hint={hint}>
      <Input
        type="password"
        variant="compact"
        value={apiKey}
        placeholder={placeholder}
        onChange={(event) => setApiKey(event.target.value)}
        onBlur={() =>
          updateSetting("cloud_api_keys", {
            ...keys,
            [provider]: apiKey.trim(),
          })
        }
        className="flex-1 max-w-[320px]"
      />
    </Row>
  );
};

/**
 * Settings that belong to the Gemini cloud models specifically — knobs no local
 * engine has.
 *
 * Rendered as bare rows inside the General tab's model-settings card, next to
 * the language selector: that card is where a model is *configured*. The Models
 * tab is where one is *chosen*, which is a different question.
 */
export const CloudModelSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting } = useSettings();

  const config = getSetting("gemini_transcribe");
  const selectedModel = getSetting("selected_model");

  const [languages, setLanguages] = useState("");
  const [vocabulary, setVocabulary] = useState("");

  useEffect(() => {
    setLanguages((config?.language_codes ?? []).join(", "));
    setVocabulary((config?.custom_vocabulary ?? []).join(", "));
  }, [config?.language_codes, config?.custom_vocabulary]);

  // ElevenLabs has no per-model knobs of its own — mode, language hints and
  // vocabulary biasing are all Gemini's API surface — so a key is the whole
  // configuration surface for it.
  if (isElevenLabsModel(selectedModel ?? undefined)) {
    return (
      <ApiKeyRow
        provider={ELEVENLABS_PROVIDER}
        placeholder="sk_…"
        hint={t("settings.models.cloud.apiKeyHintElevenLabs")}
      />
    );
  }

  if (!config) return null;

  const patch = (changes: Partial<typeof config>) =>
    updateSetting("gemini_transcribe", { ...config, ...changes });

  const smart = config.mode === "smart";
  // Google makes these mutually exclusive with smart mode, so the UI locks them
  // rather than letting the request come back as a 400.
  const timestampsLocked = smart;
  // Verified against the live service: a non-empty language list silently
  // disables smart mode's cleanup. Worth saying out loud — it reads as a bug.
  const languagesSuppressSmart = smart && parseList(languages).length > 0;

  return (
    <>
      <ApiKeyRow
        provider={GEMINI_PROVIDER}
        placeholder="AIza…"
        hint={t("settings.models.cloud.apiKeyHint")}
      />

      <Row
        label={t("settings.models.cloud.mode")}
        hint={
          smart
            ? t("settings.models.cloud.modeSmartHint")
            : t("settings.models.cloud.modeVerbatimHint")
        }
      >
        <div className="flex gap-1">
          {(["smart", "verbatim"] as GeminiTranscribeMode[]).map((option) => (
            <button
              key={option}
              onClick={() => patch({ mode: option })}
              className={`px-2.5 py-1 rounded text-xs transition-colors cursor-pointer ${
                config.mode === option
                  ? "bg-logo-primary/20 text-logo-primary"
                  : "text-text/50 hover:text-text"
              }`}
            >
              {t(`settings.models.cloud.mode_${option}`)}
            </button>
          ))}
        </div>
      </Row>

      <Row
        label={t("settings.models.cloud.languages")}
        hint={
          languagesSuppressSmart
            ? t("settings.models.cloud.languagesSuppressSmart")
            : t("settings.models.cloud.languagesHint")
        }
      >
        <Input
          variant="compact"
          value={languages}
          placeholder="ru-RU, en-US"
          onChange={(event) => setLanguages(event.target.value)}
          onBlur={() => patch({ language_codes: parseList(languages) })}
          className="flex-1 max-w-[320px]"
        />
      </Row>

      <Row
        label={t("settings.models.cloud.vocabulary")}
        hint={t("settings.models.cloud.vocabularyHint")}
      >
        <Input
          variant="compact"
          value={vocabulary}
          placeholder="10Clouds, 10CFI"
          onChange={(event) => setVocabulary(event.target.value)}
          onBlur={() => patch({ custom_vocabulary: parseList(vocabulary) })}
          className="flex-1 max-w-[320px]"
        />
      </Row>

      <div className="border-t border-mid-gray/20">
        <ToggleSwitch
          checked={config.include_custom_words}
          onChange={(value) => patch({ include_custom_words: value })}
          label={t("settings.models.cloud.includeCustomWords")}
          description={t("settings.models.cloud.includeCustomWordsHint")}
          descriptionMode="inline"
          grouped
        />
        <ToggleSwitch
          checked={config.diarization && !smart}
          onChange={(value) => patch({ diarization: value })}
          disabled={timestampsLocked}
          label={t("settings.models.cloud.diarization")}
          description={
            timestampsLocked
              ? t("settings.models.cloud.needsVerbatim")
              : t("settings.models.cloud.diarizationHint")
          }
          descriptionMode="inline"
          grouped
        />
        <ToggleSwitch
          checked={config.timestamps && !smart}
          onChange={(value) => patch({ timestamps: value })}
          disabled={timestampsLocked}
          label={t("settings.models.cloud.timestamps")}
          description={
            timestampsLocked
              ? t("settings.models.cloud.needsVerbatim")
              : t("settings.models.cloud.timestampsHint")
          }
          descriptionMode="inline"
          grouped
        />
      </div>
    </>
  );
};
