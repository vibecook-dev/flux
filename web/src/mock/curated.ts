/**
 * Hand-picked mock metadata for well-known titles in the test library, so the
 * screens look like a real household library. Loglines are original one-line
 * summaries written for this mock (no provider text is copied). Media profiles
 * pick a feature clip of the same container family as the file's extension,
 * chosen to exercise each playback path (direct, remux, audio conversion,
 * transcode). Keys are "Title|Year".
 */
export interface Curated {
  genres: string[];
  logline: string;
  runtimeMin: number;
  /** Feature clip whose probe facts the file's asset reuses. */
  profile?: string;
  addedDaysAgo?: number;
}

export const CURATED: Record<string, Curated> = {
  'Dune: Part Two|2024': { genres: ['Science Fiction', 'Adventure'], logline: 'Paul Atreides joins the Fremen of Arrakis and turns a war of revenge into something far larger than himself.', runtimeMin: 166, profile: 'mpeg2_ac3.ts', addedDaysAgo: 6 },
  'Arrival|2016': { genres: ['Science Fiction', 'Drama'], logline: 'A linguist is asked to talk to visitors whose language changes how she experiences time.', runtimeMin: 116, profile: 'mpeg4_mp3.avi' },
  'Spirited Away|2001': { genres: ['Animation', 'Fantasy'], logline: 'Trapped in a bathhouse for spirits, ten-year-old Chihiro works to free her parents and hold on to her own name.', runtimeMin: 125, profile: 'h264_multi_audio_subs.mkv' },
  'Past Lives|2023': { genres: ['Romance', 'Drama'], logline: 'Two childhood friends meet again in New York, twenty years and an ocean later.', runtimeMin: 106, profile: 'av1_opus.mkv', addedDaysAgo: 3 },
  'Blade Runner 2049|2017': { genres: ['Science Fiction', 'Thriller'], logline: 'A replicant hunter uncovers a buried secret that could erase the line between people and machines.', runtimeMin: 164, profile: 'hevc_main10_hdr10_eac3.mkv' },
  'The Matrix|1999': { genres: ['Science Fiction', 'Action'], logline: 'A hacker learns that the world he knows is a simulation, and that he may be the one to break it.', runtimeMin: 136, profile: 'hevc_main10_hdr10_eac3.mkv' },
  'Inception|2010': { genres: ['Science Fiction', 'Thriller'], logline: 'A thief who steals secrets from dreams is hired to plant an idea instead.', runtimeMin: 148, profile: 'h264_aac_moov_end.mp4' },
  'Interstellar|2014': { genres: ['Science Fiction', 'Drama'], logline: 'With Earth failing, a pilot leaves his children to look for a new home through a wormhole near Saturn.', runtimeMin: 169, profile: 'h264_aac_moov_end.mp4' },
  'Parasite|2019': { genres: ['Thriller', 'Drama'], logline: 'A struggling family talks its way into a wealthy household, one job at a time.', runtimeMin: 132 },
  'Princess Mononoke|1997': { genres: ['Animation', 'Fantasy'], logline: 'A cursed prince is caught between an ironworks town and the forest gods it is destroying.', runtimeMin: 134, profile: 'h264_truehd71.mkv' },
  'Your Name.|2016': { genres: ['Animation', 'Romance'], logline: 'A boy in Tokyo and a girl in the mountains start waking up in each other’s lives.', runtimeMin: 106, profile: 'h264_dts51.mkv' },
  'Amélie|2001': { genres: ['Comedy', 'Romance'], logline: 'A shy Parisian waitress decides to quietly fix the lives of the people around her.', runtimeMin: 122, profile: 'h264_flac.mkv' },
  'Godzilla Minus One|2023': { genres: ['Science Fiction', 'Drama'], logline: 'In ruined postwar Japan, a former pilot faces a monster and his own survivor’s guilt.', runtimeMin: 125, profile: 'hevc_4k_60mbps.mkv', addedDaysAgo: 2 },
  'Akira|1988': { genres: ['Animation', 'Science Fiction'], logline: 'In Neo-Tokyo, a biker gang member gains powers that threaten the whole city.', runtimeMin: 124 },
  'Fight Club|1999': { genres: ['Drama', 'Thriller'], logline: 'An insomniac office worker and a soap salesman start an underground club that gets out of hand.', runtimeMin: 139 },
  'Heat|1995': { genres: ['Crime', 'Thriller'], logline: 'A meticulous thief and an obsessive detective circle each other across Los Angeles.', runtimeMin: 170 },
  'Alien|1979': { genres: ['Horror', 'Science Fiction'], logline: 'The crew of a cargo ship answers a distress call and brings something back aboard.', runtimeMin: 117 },
  '2001: A Space Odyssey|1968': { genres: ['Science Fiction'], logline: 'A black monolith marks humanity’s steps from the first tools to a mission beyond Jupiter.', runtimeMin: 149 },
  'The Godfather|1972': { genres: ['Crime', 'Drama'], logline: 'The ageing head of a crime family hands power to his reluctant youngest son.', runtimeMin: 175 },
  'In the Mood for Love|2000': { genres: ['Romance', 'Drama'], logline: 'Two neighbours in 1960s Hong Kong learn their spouses are having an affair, and grow close themselves.', runtimeMin: 98 },
  'Mad Max: Fury Road|2015': { genres: ['Action', 'Adventure'], logline: 'A drifter and a rebel commander flee across the desert with a tyrant’s captive wives.', runtimeMin: 120, profile: 'h264_aac_faststart.mp4' },
  'Oppenheimer|2023': { genres: ['Drama', 'History'], logline: 'The physicist who led the Manhattan Project lives with what it made possible.', runtimeMin: 180 },
  'Everything Everywhere All at Once|2022': { genres: ['Science Fiction', 'Comedy'], logline: 'A laundromat owner in the middle of a tax audit learns she has to save the multiverse.', runtimeMin: 139 },
  'Flow|2024': { genres: ['Animation', 'Adventure'], logline: 'After a great flood, a cat shares a drifting boat with animals it would rather avoid.', runtimeMin: 85, addedDaysAgo: 1 },
  // Shows (runtime = typical episode length).
  'Severance|2022': { genres: ['Drama', 'Mystery', 'Science Fiction'], logline: 'Office workers whose memories are surgically split between work and home start investigating their employer.', runtimeMin: 55 },
  'The Bear|2022': { genres: ['Comedy', 'Drama'], logline: 'A fine-dining chef comes home to run his late brother’s chaotic Chicago sandwich shop.', runtimeMin: 30, profile: 'h264_aac.m2ts' },
  'Andor|2022': { genres: ['Science Fiction', 'Drama'], logline: 'A thief becomes a rebel in the years before the Rebellion has a name.', runtimeMin: 45, profile: 'h264_aac_faststart.mp4' },
  'Breaking Bad|2008': { genres: ['Crime', 'Drama'], logline: 'A chemistry teacher with a terminal diagnosis starts cooking methamphetamine.', runtimeMin: 47 },
  'Shōgun|2024': { genres: ['Drama', 'History'], logline: 'An English navigator shipwrecked in feudal Japan is drawn into a war between regents.', runtimeMin: 58, addedDaysAgo: 1 },
  'Arcane|2021': { genres: ['Animation', 'Fantasy'], logline: 'Two sisters end up on opposite sides of a war between a gleaming city and its undercity.', runtimeMin: 40 },
  'Chernobyl|2019': { genres: ['Drama', 'History'], logline: 'The 1986 nuclear disaster, and the people who paid to contain it.', runtimeMin: 65 },
  'Slow Horses|2022': { genres: ['Thriller', 'Comedy'], logline: 'Disgraced MI5 agents exiled to a dead-end office keep stumbling into real cases.', runtimeMin: 45 },
  'Succession|2018': { genres: ['Drama', 'Comedy'], logline: 'The children of a media mogul fight for control of the family company.', runtimeMin: 60 },
  "Frieren: Beyond Journey's End|2023": { genres: ['Animation', 'Fantasy'], logline: 'Long after her party defeats the Demon King, an elf mage sets out to understand the friends she outlived.', runtimeMin: 24 },
};

/** TMDB ids for the "Matched as" panel (mock; ids from memory, unverified). */
export const MATCHED_IDS: Record<string, number> = {
  'The Matrix|1999': 603, 'Fight Club|1999': 550, 'Inception|2010': 27205, 'Interstellar|2014': 157336,
  'The Dark Knight|2008': 155, 'Pulp Fiction|1994': 680, 'The Shawshank Redemption|1994': 278, 'The Godfather|1972': 238,
  'Dune: Part Two|2024': 693134, 'Dune|2021': 438631, 'Parasite|2019': 496243, 'Spirited Away|2001': 129,
  'Princess Mononoke|1997': 128, 'Akira|1988': 149, 'Alien|1979': 348, 'Toy Story|1995': 862, 'Jurassic Park|1993': 329,
  'Forrest Gump|1994': 13, 'Back to the Future|1985': 105, 'Gladiator|2000': 98, 'Memento|2000': 77,
  'Oppenheimer|2023': 872585, 'Blade Runner|1982': 78, '2001: A Space Odyssey|1968': 62, 'Se7en|1995': 807,
  'Heat|1995': 949, 'The Prestige|2006': 1124, 'Mad Max: Fury Road|2015': 76341, 'Arrival|2016': 329865,
  'Blade Runner 2049|2017': 335984, 'Casablanca|1942': 289, 'Psycho|1960': 539, 'Jaws|1975': 578,
  'Amélie|2001': 194, 'Past Lives|2023': 666277,
  'Breaking Bad|2008': 1396, 'Game of Thrones|2011': 1399, 'The Wire|2002': 1438, 'The Sopranos|1999': 1398,
  'Better Call Saul|2015': 60059, 'Severance|2022': 95396, 'Stranger Things|2016': 66732, 'Chernobyl|2019': 87108,
  'Friends|1994': 1668, 'The Simpsons|1989': 456, 'Arcane|2021': 94605, 'The Bear|2022': 136315, 'Andor|2022': 83867,
};
