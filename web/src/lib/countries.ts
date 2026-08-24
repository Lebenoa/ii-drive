/**
 * Country dial codes for the login form.
 *
 * Static data because no browser API exposes calling codes — `Intl` knows
 * region names but not `+66`. Kept as tuples so the table stays diffable and
 * costs ~7 KB rather than an object per row.
 */
type Row = readonly [name: string, iso2: string, dial: string];

// prettier-ignore
const ROWS: readonly Row[] = [
  ['Afghanistan', 'AF', '93'], ['Albania', 'AL', '355'], ['Algeria', 'DZ', '213'],
  ['American Samoa', 'AS', '1684'], ['Andorra', 'AD', '376'], ['Angola', 'AO', '244'],
  ['Anguilla', 'AI', '1264'], ['Antigua and Barbuda', 'AG', '1268'], ['Argentina', 'AR', '54'],
  ['Armenia', 'AM', '374'], ['Aruba', 'AW', '297'], ['Australia', 'AU', '61'],
  ['Austria', 'AT', '43'], ['Azerbaijan', 'AZ', '994'], ['Bahamas', 'BS', '1242'],
  ['Bahrain', 'BH', '973'], ['Bangladesh', 'BD', '880'], ['Barbados', 'BB', '1246'],
  ['Belarus', 'BY', '375'], ['Belgium', 'BE', '32'], ['Belize', 'BZ', '501'],
  ['Benin', 'BJ', '229'], ['Bermuda', 'BM', '1441'], ['Bhutan', 'BT', '975'],
  ['Bolivia', 'BO', '591'], ['Bosnia and Herzegovina', 'BA', '387'], ['Botswana', 'BW', '267'],
  ['Brazil', 'BR', '55'], ['British Virgin Islands', 'VG', '1284'], ['Brunei', 'BN', '673'],
  ['Bulgaria', 'BG', '359'], ['Burkina Faso', 'BF', '226'], ['Burundi', 'BI', '257'],
  ['Cambodia', 'KH', '855'], ['Cameroon', 'CM', '237'], ['Canada', 'CA', '1'],
  ['Cape Verde', 'CV', '238'], ['Cayman Islands', 'KY', '1345'],
  ['Central African Republic', 'CF', '236'], ['Chad', 'TD', '235'], ['Chile', 'CL', '56'],
  ['China', 'CN', '86'], ['Colombia', 'CO', '57'], ['Comoros', 'KM', '269'],
  ['Congo (DRC)', 'CD', '243'], ['Congo (Republic)', 'CG', '242'], ['Cook Islands', 'CK', '682'],
  ['Costa Rica', 'CR', '506'], ['Côte d’Ivoire', 'CI', '225'], ['Croatia', 'HR', '385'],
  ['Cuba', 'CU', '53'], ['Curaçao', 'CW', '599'], ['Cyprus', 'CY', '357'],
  ['Czechia', 'CZ', '420'], ['Denmark', 'DK', '45'], ['Djibouti', 'DJ', '253'],
  ['Dominica', 'DM', '1767'], ['Dominican Republic', 'DO', '1809'], ['Ecuador', 'EC', '593'],
  ['Egypt', 'EG', '20'], ['El Salvador', 'SV', '503'], ['Equatorial Guinea', 'GQ', '240'],
  ['Eritrea', 'ER', '291'], ['Estonia', 'EE', '372'], ['Eswatini', 'SZ', '268'],
  ['Ethiopia', 'ET', '251'], ['Faroe Islands', 'FO', '298'], ['Fiji', 'FJ', '679'],
  ['Finland', 'FI', '358'], ['France', 'FR', '33'], ['French Guiana', 'GF', '594'],
  ['French Polynesia', 'PF', '689'], ['Gabon', 'GA', '241'], ['Gambia', 'GM', '220'],
  ['Georgia', 'GE', '995'], ['Germany', 'DE', '49'], ['Ghana', 'GH', '233'],
  ['Gibraltar', 'GI', '350'], ['Greece', 'GR', '30'], ['Greenland', 'GL', '299'],
  ['Grenada', 'GD', '1473'], ['Guadeloupe', 'GP', '590'], ['Guam', 'GU', '1671'],
  ['Guatemala', 'GT', '502'], ['Guinea', 'GN', '224'], ['Guinea-Bissau', 'GW', '245'],
  ['Guyana', 'GY', '592'], ['Haiti', 'HT', '509'], ['Honduras', 'HN', '504'],
  ['Hong Kong', 'HK', '852'], ['Hungary', 'HU', '36'], ['Iceland', 'IS', '354'],
  ['India', 'IN', '91'], ['Indonesia', 'ID', '62'], ['Iran', 'IR', '98'],
  ['Iraq', 'IQ', '964'], ['Ireland', 'IE', '353'], ['Israel', 'IL', '972'],
  ['Italy', 'IT', '39'], ['Jamaica', 'JM', '1876'], ['Japan', 'JP', '81'],
  ['Jordan', 'JO', '962'], ['Kazakhstan', 'KZ', '7'], ['Kenya', 'KE', '254'],
  ['Kiribati', 'KI', '686'], ['Kosovo', 'XK', '383'], ['Kuwait', 'KW', '965'],
  ['Kyrgyzstan', 'KG', '996'], ['Laos', 'LA', '856'], ['Latvia', 'LV', '371'],
  ['Lebanon', 'LB', '961'], ['Lesotho', 'LS', '266'], ['Liberia', 'LR', '231'],
  ['Libya', 'LY', '218'], ['Liechtenstein', 'LI', '423'], ['Lithuania', 'LT', '370'],
  ['Luxembourg', 'LU', '352'], ['Macao', 'MO', '853'], ['Madagascar', 'MG', '261'],
  ['Malawi', 'MW', '265'], ['Malaysia', 'MY', '60'], ['Maldives', 'MV', '960'],
  ['Mali', 'ML', '223'], ['Malta', 'MT', '356'], ['Marshall Islands', 'MH', '692'],
  ['Martinique', 'MQ', '596'], ['Mauritania', 'MR', '222'], ['Mauritius', 'MU', '230'],
  ['Mexico', 'MX', '52'], ['Micronesia', 'FM', '691'], ['Moldova', 'MD', '373'],
  ['Monaco', 'MC', '377'], ['Mongolia', 'MN', '976'], ['Montenegro', 'ME', '382'],
  ['Montserrat', 'MS', '1664'], ['Morocco', 'MA', '212'], ['Mozambique', 'MZ', '258'],
  ['Myanmar', 'MM', '95'], ['Namibia', 'NA', '264'], ['Nauru', 'NR', '674'],
  ['Nepal', 'NP', '977'], ['Netherlands', 'NL', '31'], ['New Caledonia', 'NC', '687'],
  ['New Zealand', 'NZ', '64'], ['Nicaragua', 'NI', '505'], ['Niger', 'NE', '227'],
  ['Nigeria', 'NG', '234'], ['North Korea', 'KP', '850'], ['North Macedonia', 'MK', '389'],
  ['Norway', 'NO', '47'], ['Oman', 'OM', '968'], ['Pakistan', 'PK', '92'],
  ['Palau', 'PW', '680'], ['Palestine', 'PS', '970'], ['Panama', 'PA', '507'],
  ['Papua New Guinea', 'PG', '675'], ['Paraguay', 'PY', '595'], ['Peru', 'PE', '51'],
  ['Philippines', 'PH', '63'], ['Poland', 'PL', '48'], ['Portugal', 'PT', '351'],
  ['Puerto Rico', 'PR', '1787'], ['Qatar', 'QA', '974'], ['Romania', 'RO', '40'],
  ['Russia', 'RU', '7'], ['Rwanda', 'RW', '250'], ['Réunion', 'RE', '262'],
  ['Saint Kitts and Nevis', 'KN', '1869'], ['Saint Lucia', 'LC', '1758'],
  ['Saint Vincent and the Grenadines', 'VC', '1784'], ['Samoa', 'WS', '685'],
  ['San Marino', 'SM', '378'], ['Saudi Arabia', 'SA', '966'], ['Senegal', 'SN', '221'],
  ['Serbia', 'RS', '381'], ['Seychelles', 'SC', '248'], ['Sierra Leone', 'SL', '232'],
  ['Singapore', 'SG', '65'], ['Sint Maarten', 'SX', '1721'], ['Slovakia', 'SK', '421'],
  ['Slovenia', 'SI', '386'], ['Solomon Islands', 'SB', '677'], ['Somalia', 'SO', '252'],
  ['South Africa', 'ZA', '27'], ['South Korea', 'KR', '82'], ['South Sudan', 'SS', '211'],
  ['Spain', 'ES', '34'], ['Sri Lanka', 'LK', '94'], ['Sudan', 'SD', '249'],
  ['Suriname', 'SR', '597'], ['Sweden', 'SE', '46'], ['Switzerland', 'CH', '41'],
  ['São Tomé and Príncipe', 'ST', '239'], ['Syria', 'SY', '963'], ['Taiwan', 'TW', '886'],
  ['Tajikistan', 'TJ', '992'], ['Tanzania', 'TZ', '255'], ['Thailand', 'TH', '66'],
  ['Timor-Leste', 'TL', '670'], ['Togo', 'TG', '228'], ['Tonga', 'TO', '676'],
  ['Trinidad and Tobago', 'TT', '1868'], ['Tunisia', 'TN', '216'],
  ['Turkmenistan', 'TM', '993'], ['Turks and Caicos Islands', 'TC', '1649'],
  ['Tuvalu', 'TV', '688'], ['Türkiye', 'TR', '90'], ['Uganda', 'UG', '256'],
  ['Ukraine', 'UA', '380'], ['United Arab Emirates', 'AE', '971'],
  ['United Kingdom', 'GB', '44'], ['United States', 'US', '1'], ['Uruguay', 'UY', '598'],
  ['Uzbekistan', 'UZ', '998'], ['Vanuatu', 'VU', '678'], ['Vatican City', 'VA', '379'],
  ['Venezuela', 'VE', '58'], ['Vietnam', 'VN', '84'], ['Yemen', 'YE', '967'],
  ['Zambia', 'ZM', '260'], ['Zimbabwe', 'ZW', '263'],
];

export type Country = {
  readonly name: string;
  readonly iso2: string;
  readonly dial: string;
  /** Regional-indicator pair. Renders as a flag wherever the platform has the glyphs. */
  readonly flag: string;
  /** Pre-lowercased haystack, so filtering does no per-keystroke allocation. */
  readonly search: string;
};

/** ISO 3166-1 alpha-2 -> regional indicator symbols, e.g. "TH" -> 🇹🇭. */
export function flagOf(iso2: string): string {
  return String.fromCodePoint(
    ...[...iso2.toUpperCase()].map((c) => 0x1f1e6 + c.charCodeAt(0) - 65),
  );
}

export const COUNTRIES: readonly Country[] = ROWS.map(([name, iso2, dial]) => ({
  name,
  iso2,
  dial,
  flag: flagOf(iso2),
  search: `${name} ${iso2} ${dial} +${dial}`.toLowerCase(),
})).sort((a, b) => a.name.localeCompare(b.name));

/** Static, string-keyed, read-only: a plain object beats a Map here. */
const BY_ISO: Record<string, Country | undefined> = Object.fromEntries(
  COUNTRIES.map((c) => [c.iso2, c]),
);

/** Longest dial code first, so +1684 wins over +1 when matching a prefix. */
const BY_DIAL_LENGTH = [...COUNTRIES].sort((a, b) => b.dial.length - a.dial.length);

export function countryOf(iso2: string): Country | undefined {
  return BY_ISO[iso2.toUpperCase()];
}

/**
 * The country the browser's locale points at, so the common case needs no
 * interaction. Falls back to the US when the locale carries no region (e.g.
 * a bare "en") or names one we have no dial code for.
 */
export function guessCountry(): Country {
  const fallback = countryOf('US') ?? COUNTRIES[0];
  if (typeof navigator === 'undefined') return fallback;
  const tags = navigator.languages?.length ? navigator.languages : [navigator.language];
  for (const tag of tags) {
    if (!tag) continue;
    // `Intl.Locale` throws on malformed tags, which we simply skip.
    let region: string | undefined;
    try {
      region = new Intl.Locale(tag).region ?? undefined;
    } catch {
      region = undefined;
    }
    const hit = region ? countryOf(region) : undefined;
    if (hit) return hit;
  }
  return fallback;
}

/**
 * Splits what the user typed into a country and a national number.
 *
 * Two shapes have to work, because people paste as often as they type:
 * a full international number ("+66 98 896 2019", or "0066…") selects its
 * country and keeps the rest, while a plain national number keeps whatever
 * country is already chosen.
 *
 * A leading zero is a *trunk prefix* — how you dial your own country from
 * inside it — and is never part of the international form, so it is dropped.
 * Telegram would reject "+66 0988…".
 */
export function splitNumber(
  raw: string,
  current: Country,
): { country: Country; national: string } {
  const digits = raw.replace(/\D/g, '');
  const trimmed = raw.trim();
  // "+" or a literal "00" prefix mean "a country code follows". Checked on
  // the raw text, not the digit-crunched form: internal separators could
  // otherwise fabricate a leading "00" out of a plain national number.
  const international = trimmed.startsWith('+') || trimmed.startsWith('00');
  const body = digits.startsWith('00') && international ? digits.slice(2) : digits;

  if (international && body.length > 0) {
    // Several entries share a dial code (+1 US/Canada, +7 RU/KZ). When the
    // current selection dials the same code, keep it, so an American typing
    // "+1…" is not told they are in Canada.
    const matches = (c: Country) => body.startsWith(c.dial) && body.length > c.dial.length;
    const hit =
      BY_DIAL_LENGTH.find((c) => matches(c) && c.iso2 === current.iso2) ??
      BY_DIAL_LENGTH.find((c) => matches(c) && c.iso2 === guessCountry().iso2) ??
      BY_DIAL_LENGTH.find(matches);
    if (hit) return { country: hit, national: stripTrunk(body.slice(hit.dial.length)) };
    // Digits so far are still inside a dial code (mid-typing): keep them as
    // the national part rather than guessing a country.
  }
  return { country: current, national: stripTrunk(body) };
}

/**
 * Drops the national trunk prefix — at most one zero. Italy is the reason
 * this is not `/^0+/`: its landline numbers keep the leading zero even
 * internationally (+39 06…), so a greedy strip mangles them.
 */
export function stripTrunk(digits: string): string {
  return digits.replace(/^0/, '');
}

/**
 * The value the API wants: "+" + dial code + national digits.
 *
 * Deliberately does NOT re-run `stripTrunk`: the caller has already applied
 * it at the layer that knew whether a zero was a trunk prefix or part of the
 * number (Italy keeps hers).
 */
export function toE164(country: Country, national: string): string {
  return `+${country.dial}${national.replace(/\D/g, '')}`;
}

let flagSupport: boolean | null = null;

/**
 * Whether this platform actually draws flag emoji. Measured once, lazily:
 * the first call may happen during SSR, where the answer is trivially no,
 * and that must not poison the client's answer.
 *
 * Windows ships no flag glyphs, so a regional-indicator pair falls back to
 * the two letters ("TH"). Measuring is the only way to know: the pair is
 * markedly narrower as one flag than as two letters. Callers show an ISO
 * chip instead when this is false, which beats letters pretending to be a
 * flag.
 */
export function flagsRender(): boolean {
  if (flagSupport !== null) return flagSupport;
  if (typeof document === 'undefined') return false;
  const ctx = document.createElement('canvas').getContext('2d');
  if (!ctx) return false;
  ctx.font = '24px sans-serif';
  const flag = ctx.measureText(flagOf('TH')).width;
  const letters = ctx.measureText('TH').width;
  // Same width means the glyphs are the letters themselves.
  flagSupport = flag > 0 && flag < letters * 0.9;
  return flagSupport;
}
