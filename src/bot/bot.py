import discord
import traceback
import random
from lib.utils import (
    get_data,
    get_current_status,
    ClipUrl,
    get_bug_report_path
)

from lib.clip import Clip

VALUES = get_data("values")
DISCORD_TOKEN : str = VALUES["discordToken"]
UPLOAD_LIMIT : int = VALUES["uploadLimit"]
IS_ARCHIVE : bool = VALUES["archive"]

#Declaring intents, must also be configured from Discord portal, see readme
intents = discord.Intents.all()
intents.members = True

#Discord, Discord-Embed
client = discord.Client(intents = intents)
embeded = discord.Embed()


#Functions ---------
async def on_valid_url(message):

    clip_url = ClipUrl(message.content.strip())

    if clip_url.platform == 'Invalid': return

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

    notice = await message.channel.send(message.author.mention +
    f'\n`{clip_url.platform}` video detected. {random.choice(action_phrases)}.')
    
    async with message.channel.typing():
        clip = Clip(clip_url)
        
        if clip.download():

            if clip.size >= UPLOAD_LIMIT:
                clip.move('incomplete')
                await client.change_presence(
                    activity=discord.Activity(
                    type = discord.ActivityType.watching,
                    name = f'& transcoding {clip.file_name}'
                    ))
                await clip.transcode(UPLOAD_LIMIT)
            else:
                clip.move('completed')

        await client.change_presence(
            activity=discord.Activity(
            type = discord.ActivityType.watching,
            name = f'& waiting for links'
            ))

        if clip.check_fail():
            await notice.edit(content=f'Invalid link. Failed while {random.choice(action_phrases).lower()}.')
            return

        await notice.delete()
        clip_msg = await message.channel.send(f'{message.author.mention}',
        file=discord.File(clip.full_path))
        clip.update_for_upload()
    
        if IS_ARCHIVE:
            clip.archive()
        else:
            clip.delete()



#Client events -------
@client.event
async def on_ready() -> None: #When bot starts up, adds log
    stat = get_current_status()
    print(f"Bot is ready to party, logged in as {client.user}. Currently {stat}")
    await client.change_presence(
        activity=discord.Activity(
            type = discord.ActivityType.watching,
            name = f'& {stat}'
            ))
    return

@client.event
async def on_message(message) -> None: #On every incoming message, run the below code

    if message.author == client.user: #If message sender is another bot, or itself, or a non-user
        return

    elif ClipUrl.eval_msg(message):
        await on_valid_url(message)
        
    elif message.content.lower().startswith('!dc test'):
        await message.channel.send("Testing!")

@client.event
async def on_error(event, *args, **kwargs):

    err = traceback.format_exc()
    channel = args[0].channel
    report_path = get_bug_report_path(channel.name)
    with open(report_path, 'w') as er:
        er.write(str(err))
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

if __name__ == "__main__":
    client.run(DISCORD_TOKEN)
