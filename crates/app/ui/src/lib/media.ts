// The containers and codecs a profile's output may name, as the engine spells them

import type {
	AudioCodec,
	AudioContainer,
	ImageContainer,
	VideoCodec,
	VideoContainer
} from './api/types';

export const VIDEO_CONTAINERS: VideoContainer[] = ['mp4', 'mov', 'mkv', 'webm'];
export const VIDEO_CODECS: VideoCodec[] = ['h264', 'h265', 'vp9', 'vp8', 'av1'];
export const AUDIO_CODECS: AudioCodec[] = ['aac', 'mp3', 'opus', 'vorbis', 'flac'];
export const AUDIO_CONTAINERS: AudioContainer[] = ['m4a', 'mp3', 'ogg', 'opus', 'flac', 'wav'];
export const IMAGE_CONTAINERS: ImageContainer[] = ['jpeg', 'png', 'webp', 'gif'];

const LABELS: Record<string, string> = {
	mp4: 'MP4',
	mov: 'MOV',
	mkv: 'MKV',
	webm: 'WebM',
	h264: 'H.264',
	h265: 'H.265',
	vp9: 'VP9',
	vp8: 'VP8',
	av1: 'AV1',
	aac: 'AAC',
	mp3: 'MP3',
	opus: 'Opus',
	vorbis: 'Vorbis',
	flac: 'FLAC',
	m4a: 'M4A',
	ogg: 'Ogg',
	wav: 'WAV',
	jpeg: 'JPEG',
	png: 'PNG',
	webp: 'WebP',
	gif: 'GIF'
};

/** The name a container or codec goes by */
export function formatLabel(id: string): string {
	return LABELS[id] ?? id.toUpperCase();
}
