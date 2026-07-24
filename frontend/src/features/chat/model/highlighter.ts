import bash from "shiki/langs/bash.mjs";
import c from "shiki/langs/c.mjs";
import cpp from "shiki/langs/cpp.mjs";
import csharp from "shiki/langs/csharp.mjs";
import css from "shiki/langs/css.mjs";
import csv from "shiki/langs/csv.mjs";
import docker from "shiki/langs/docker.mjs";
import dotenv from "shiki/langs/dotenv.mjs";
import go from "shiki/langs/go.mjs";
import html from "shiki/langs/html.mjs";
import ini from "shiki/langs/ini.mjs";
import java from "shiki/langs/java.mjs";
import javascript from "shiki/langs/javascript.mjs";
import json from "shiki/langs/json.mjs";
import jsx from "shiki/langs/jsx.mjs";
import kotlin from "shiki/langs/kotlin.mjs";
import lua from "shiki/langs/lua.mjs";
import make from "shiki/langs/make.mjs";
import markdown from "shiki/langs/markdown.mjs";
import nix from "shiki/langs/nix.mjs";
import perl from "shiki/langs/perl.mjs";
import php from "shiki/langs/php.mjs";
import properties from "shiki/langs/properties.mjs";
import python from "shiki/langs/python.mjs";
import ruby from "shiki/langs/ruby.mjs";
import rust from "shiki/langs/rust.mjs";
import scss from "shiki/langs/scss.mjs";
import sql from "shiki/langs/sql.mjs";
import swift from "shiki/langs/swift.mjs";
import toml from "shiki/langs/toml.mjs";
import tsx from "shiki/langs/tsx.mjs";
import typescript from "shiki/langs/typescript.mjs";
import vue from "shiki/langs/vue.mjs";
import xml from "shiki/langs/xml.mjs";
import yaml from "shiki/langs/yaml.mjs";
import {
  createCssVariablesTheme,
  createHighlighterCore,
  type LanguageRegistration,
} from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";

import sidexGrammarSource from "./sidex.tmLanguage.json?raw";

const sidex = {
  ...(JSON.parse(sidexGrammarSource) as object),
  name: "sidex",
  aliases: ["Sidex"],
} as LanguageRegistration;

const languages = [
  bash,
  c,
  cpp,
  csharp,
  css,
  csv,
  docker,
  dotenv,
  go,
  html,
  ini,
  java,
  javascript,
  json,
  jsx,
  kotlin,
  lua,
  make,
  markdown,
  nix,
  perl,
  php,
  properties,
  python,
  ruby,
  rust,
  scss,
  sidex,
  sql,
  swift,
  toml,
  tsx,
  typescript,
  vue,
  xml,
  yaml,
];

const syntaxTheme = createCssVariablesTheme({
  name: "tascarrel",
  variablePrefix: "--syntax-",
  fontStyle: false,
});

syntaxTheme.tokenColors = [
  ...(syntaxTheme.tokenColors ?? []),
  {
    scope: ["constant.numeric"],
    settings: { foreground: "var(--syntax-token-number)" },
  },
  {
    scope: ["entity.name.type", "entity.name.sidex", "support.type"],
    settings: { foreground: "var(--syntax-token-type)" },
  },
  {
    scope: ["variable.other.field", "support.type.property-name"],
    settings: { foreground: "var(--syntax-token-property)" },
  },
  {
    scope: ["variable.other.attribute", "entity.other.attribute-name"],
    settings: { foreground: "var(--syntax-token-attribute)" },
  },
  {
    scope: ["variable.parameter"],
    settings: { foreground: "var(--syntax-token-parameter)" },
  },
  {
    scope: ["punctuation"],
    settings: { foreground: "var(--syntax-token-punctuation)" },
  },
];

const highlighter = createHighlighterCore({
  themes: [syntaxTheme],
  langs: languages,
  engine: createJavaScriptRegexEngine(),
});

export async function highlightCode(code: string, language: string): Promise<string> {
  const instance = await highlighter;
  const loaded = instance.getLoadedLanguages();
  return instance.codeToHtml(code, {
    lang: loaded.includes(language) ? language : "text",
    theme: "tascarrel",
  });
}
