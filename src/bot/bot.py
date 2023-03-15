import os
import sys

# SYSPATH REQUIRES APPENDING AFTER LAUNCHING AS SUBPROC
sys.path.append(os.path.abspath(os.path.join(os.path.dirname(__file__), '..')))

import socketio
import discord
import traceback
import random
import argparse

from collections import defaultdict

from lib.utils import (
    get_bug_report_path
)

from lib.clip import Clip

# SUBPROC CMD ARGS
parser = argparse.ArgumentParser()
parser.add_argument('--host', help='the websocket host', required=True)
parser.add_argument('--token', help='discord token', required=True)
args = parser.parse_args()

# CONFIGURATION (PULLED FROM DB/SERVER)
CONFIG = defaultdict(None)

def update_configuration(data):
    CONFIG['DISCORD_TOKEN'] = data['token']
    CONFIG['UPLOAD_SIZE_LIMIT'] = data['upload_size_limit']
    CONFIG['IS_ARCHIVE'] = data['is_archive']
    CONFIG['IS_DEBUG'] = data['is_debug']
    CONFIG['IS_DELETE'] = data['is_delete']
    CONFIG['IS_MENTION_USER'] = data['is_mention_user']
    CONFIG['IS_MENTION_MENTIONED'] = data['is_mention_mentioned']
    CONFIG['IS_INCLUDE_MSG'] = data['is_include_msg']
    CONFIG['IS_ONLY_CLIP'] = data['is_only_clip']
    CONFIG['IS_INSTAGRAM'] = data['is_instagram']
    CONFIG['IS_TIKTOK'] = data['is_tiktok']

async def request_configuration():
    await sio.emit('bot_connect', { 'message': 'asking server for configuration settings' }, callback=update_configuration)

# DECLARING INTENTS (SEE README FOR CONTRIBUTORS)
intents = discord.Intents.all()
intents.members = True

# INSTANTIATING DISCORD CLIENT
client = discord.Client(intents = intents)
embeded = discord.Embed()

# SOCKETIO CONFIG
sio = socketio.AsyncClient()
async def socket_start():
    await sio.connect(f'http://{args.host}')
    await request_configuration()

# BEGIN FUNCTIONS -----

async def change_bot_presence(presence):
    await client.change_presence(
        activity=discord.Activity(
        type = discord.ActivityType.watching,
        name = f'& {presence}'
        ))
    await sio.emit('bot_change_presence', {
        'presence': presence
    })

async def on_startup():
    await socket_start()
    stat = 'waiting for links'
    print(f"Bot is ready to party, logged in as {client.user}. Currently {stat}")
    await change_bot_presence(stat)
    await sio.emit('bot_started', {
        'success': True,
        'status': stat})
    return

async def on_valid_url(message, url):
    
    clip = Clip(message, url, CONFIG, callback=change_bot_presence, socket=sio)

    await clip.initialize()
    
    if clip.platform in ['Invalid'] and not CONFIG.get('IS_DEBUG', False):
        return

    action_phrases = [
        'Casting spells',
        'Working the magic',
        'Asking friends for help with this',
        'Throwing this in the trash... jk',
        'Showing my mom',
        'Showing my grandma',
        'Petting my dog',
        'Showering my unicorn',
        'Running this through Mikoshi',
        'Crying a lil',
        'Laughed so hard I literally died',
        'Slapping a fish',
        'Comitting unforgivable curses',
        'Fighting off FBI agents',
        'Eating Crème fraîche',
        'Calling my dentist'
    ]

    # ACKNOWLEDGING VALID URL
    notice = await message.channel.send(message.author.mention +
    f'\n`{clip.platform}` video detected. {random.choice(action_phrases)}.')
    
    # START "TYPING"
    async with message.channel.typing():

        # CHECK ARCHIVES FOR PRE EXISTING FILE
        if not await clip.check_archives():
            await clip.download()

        # CHECK SIZE TO DETERMINE IF TRANSCODE NEEDED
        if not await clip.check_size():
            await clip.transcode()

        # CHECK THAT FILE EXISTS AT PATH BEFORE UPLOAD
        if await clip.check_exists():
            await clip.upload(notice, discord)
        elif CONFIG.get('IS_DEBUG', False):
            await notice.edit(content=f'Invalid link. Failed while {random.choice(action_phrases)}.')
            return

        # IF SETTINGS ALLOW ARCHIVING, DO ARCHIVE ELSE DELETE FILE
        if CONFIG.get('IS_ARCHIVE', True):
            await clip.archive()
        else:
            await clip.delete(record=True)

        # IF SETTINGS SPECIFY MESSAGE DELETION, DO DELETE MESSAGE
        if CONFIG.get('IS_DELETE', True):
            await message.delete()

        # ADDING TO DB
        await clip.generate_file()

# BEGIN SOCKET EVENTS -----

@sio.on('config_updated')
async def refresh_configs(config):
    await request_configuration()


# BEGIN CLIENT EVENTS -----

@client.event
async def on_ready() -> None:
    await on_startup()

@client.event
async def on_message(message) -> None:
    if message.author == client.user:
        return
        
    url = Clip.eval_msg(message, CONFIG)
    if url:
        # REFRESH CONFIG VALUES
        await request_configuration()
        await on_valid_url(message, url)
        
    elif message.content.lower().startswith('!dc test'):
        await message.channel.send("Testing!")
        await sio.emit('test', {'msg': 'Test message from Discord bot'})

@client.event
async def on_error(event, *args, **kwargs):

    err = traceback.format_exc()
    channel = args[0].channel
    report_path = get_bug_report_path(channel.name)
    with open(report_path, 'w') as er:
        er.write(str(err))
    if CONFIG.get('IS_DEBUG', False):
        embeded = discord.Embed(title="Error Occured :(",
        url='https://github.com/nickheyer/DiscoClip/issues/new',
        description='\nError occured while sending file. ' +
                    'Consider copy and pasting the below ' +
                    'log file into a bug report ' +
                    'using the below link.\n\nThank you!',
        color=0x966FD6)
        embeded.set_author(name=str(client.user),
        icon_url=client.user.display_avatar)
        embeded.add_field(name='Submit Bug Report:',
        value='https://github.com/nickheyer/DiscoClip/issues/new',
        inline=False)
        embeded.add_field(name='Error Log:',
        value=f'```{str(err)[:256]}...```',
        inline=False)
        await channel.send(embed=embeded)
        await channel.send(file=discord.File(report_path))

# START APP -----

if __name__ == "__main__":
    try:
        client.run(args.token)
    except:
        raise Exception('Bot unable to start. Is Discord token valid?')
