document.addEventListener('DOMContentLoaded', (event) => {
     
    
    // ------------------ STARTUP EVENTS -----------------------------

    const socket = io.connect(window.location.origin)

    socket.on('client_info', (data) => {
        console.log('Current Configuration: ', data);
        updateCurrentStatus(data.state);
        updateSwitches(data.state);
        updateConfigFields(data.config);
        updateDescription(data.config.fields);
        updateLog(data.log);
    });
    
    socket.emit('client_connect', {
        message: 'hello server'
    });


    // ------------------ CONFIGURATION EVENTS -----------------------------


    socket.on('config_updated', (data) => {
        const popOver = document.getElementById('offcanvas');
        const isPopVisible = window.getComputedStyle(popOver).getPropertyValue('visibility') === 'visible' ? true : false;
        if (data.error) {
            alert(data.error);
        } else if (isPopVisible) {
            document.getElementById('close-form').click();
            updateDescription(data);
        } 
    });

    const generateCharFormField = (parentTag, val, fieldName, verboseFieldName) => {
        parentTag.innerHTML += `
        <div>
            <label for="${fieldName}" class="form-label">${verboseFieldName}</label>
            <div class="input-group mb-3">
                <input type="text" class="form-control input-format-1" id="${fieldName}" name="cred-form-field" placeholder="${verboseFieldName}"
                aria-label="${verboseFieldName}" aria-describedby="field-${verboseFieldName}" value="${val}">
            </div>
        </div>`
    };

    const generateIntFormField = (parentTag, val, fieldName, verboseFieldName) => {
        parentTag.innerHTML += `
        <div>
            <label for="${fieldName}" class="form-label">${verboseFieldName}</label>
            <div class="input-group mb-3 input-group-number">
                <input type="text" class="form-control input-format-1" id="${fieldName}" name="cred-form-field" placeholder="${verboseFieldName}"
                    aria-label="${verboseFieldName}" aria-describedby="field-${verboseFieldName}" value="${val}" inputmode="numeric"
                    onInput="this.value = this.value.replace(/[^0-9]/g, '')" required>
            </div>
        </div>`
    };

    const generateFormSwitch = (parentTag, val, fieldName, verboseFieldName) => {
        parentTag.innerHTML += `
        <div class="form-check form-switch">
            <input class="form-check-input input-format-1" type="checkbox" role="switch" name="cred-form-field" id="${fieldName}" ${(val) ? 'checked' : ''}>
            <label class="form-check-label" for="${fieldName}">${verboseFieldName}</label>
        </div>`
    };

    const updateDescription = (fields) => {
        const uploadSize = fields.upload_size_limit;
        document.getElementById('title-header').textContent = `"A bot that monitors for TikTok & Instagram video links within messages, downloads the clip, transcodes it under ${uploadSize}mb*, and posts to chat."`;
        document.getElementById('subtitle-header').textContent = `*Based on Discord's (or your own) file-size upload limit of ${uploadSize}mb`;
    };

    const updateConfigFields = (data) => {
        const configContainer = document.getElementById('config-container');
        const configFields = Object.keys(data.fields);

        configFields.forEach((field) => {

            if(field && field !== 'id') {
                const verboseName = data.verbose[field];
                const fieldType = data.types[field];
                const currentVal = data.fields[field] ? data.fields[field] : '';
                let fieldGen;
                switch(fieldType) {
                    case 'VARCHAR':
                        fieldGen = generateCharFormField;
                        break;
                    case 'INT':
                        fieldGen = generateIntFormField;
                        break;
                    case 'BOOL':
                        fieldGen = generateFormSwitch;
                        break;
                    default:
                        break;
                }
                fieldGen(
                    configContainer,
                    currentVal,
                    field,
                    verboseName
                );
                
            }
            
        });
    };

    const getConfigFromFields = () => {
        const fieldData = document.getElementsByName('cred-form-field');
        const preparedData = {};
        fieldData.forEach((fieldInput) => {
            if (!fieldInput.value) {
                return null;
            } else if (fieldInput.type === 'checkbox') {
                preparedData[fieldInput.id] = fieldInput.checked;
            } else {
                preparedData[fieldInput.id] = fieldInput.value;
            }
        });
        return preparedData;
    };

    const saveConfigInFields = () => {
        const data = getConfigFromFields();
        socket.emit('update_config', data);

    };

    const saveButton =  document.getElementById('cred-save-button');
    saveButton.onclick = (event) => {
        saveConfigInFields();
    };

    
    // ------------------ STATE EVENTS -----------------------------
    
    const serverSwitch = document.getElementById("server-io-switch");
    const botSwitch = document.getElementById("discord-io-switch");

    socket.on('update_status', (data) => {
        updateCurrentStatus(data);
    });

    const updateCurrentStatus = (data) => {
        const statusBox = document.getElementById('status-box');
        statusBox.innerHTML = data['current_activity'];
    };

    const updateSwitches = (data) => {
        serverSwitch.checked = data['app_state'];
        botSwitch.checked = data['bot_state'];
    };

    // Start shutdown of server, shutsdown after 5 seconds
    const serverSwitchShutdown = (sw) => {
        sw.className = 'form-check-input custom-check-loading';
        // Save current status to global if cancelled
        window.status = document.getElementById('status-box').innerHTML;
        updateCurrentStatus({ current_activity: 'Starting shutdown. Cancel by moving switch to \'on\' position.' })
        // Starting timer that allows cancellation of shutdown
        window.serverSwitchTimer = setTimeout(() => {
            sw.className = 'form-check-input custom-check';
            botSwitch.checked = false;
            socket.emit('server_off', {
                message: 'SERVEROFF'
            }); 
            location.reload(); 
        }, 5000);
    };

    // Cancels shutdown of server
    const clearServerShutdown = (sw) => {
        // If timer exists and previous status being held in memory
        if (window.serverSwitchTimer && window.status) {
            // Revert class to default (green glow)
            sw.className = 'form-check-input custom-check';
            clearTimeout(window.serverSwitchTimer);
            updateCurrentStatus({ current_activity: window.status });
            window.serverSwitchTimer = null;
            window.status = null;
        } else {
            alert('Server shutdown was not in progress.')
            sw.checked = true;
        }  
    };

    // On server switch event
    $(serverSwitch).change(async function() {
        if (!this.checked) {
            serverSwitchShutdown(this);
        } else {
            clearServerShutdown(this);
        }
    });

    // Shutdown bot and re-enable switch after one second
    const botSwitchShutdown = (sw) => {
        setTimeout(() => {
            socket.emit('bot_off', {
                message: 'BOTOFF'
            });
            sw.disabled = false;
            updateCurrentStatus({ current_activity: 'Offline' })
        }, 1000)
    };

    // Begins startup process. Issues startup command to server, expecting eventual response (bot_on_finished)
    const botSwitchStartup = (sw) => {
        sw.className = 'form-check-input custom-check-loading';
        socket.emit('bot_on', {
            message: 'BOTON'
        }, (resp) => {
            if (!resp.success) {
                alert(resp.error);
                botSwitch.disabled = false;
                botSwitch.checked = false;
                botSwitch.className = 'form-check-input custom-check-failed';
            } else {
                updateCurrentStatus({ current_activity: 'Starting up bot, please wait.' })
            }
        });
        
    };

    // On bot switch event
    $(botSwitch).change(async function() {
        this.disabled = true;
        if (!this.checked) {
            botSwitchShutdown(this);       
        } else {
            botSwitchStartup(this);
        }
    });
    
    // Response from server after finishing/err on bot startup
    socket.on('bot_on_finished', (data) => {
        botSwitch.disabled = false;
        if (data.error) {
            alert(data.error);
            botSwitch.checked = false;
            botSwitch.className = 'form-check-input custom-check-failed';
            updateCurrentStatus({ current_activity: 'Offline' });
        } else if (data.success) {
            botSwitch.className = 'form-check-input custom-check';
            updateCurrentStatus({ current_activity: data.status });
        }
    });

    // Response from server after finishing/err on bot startup
    socket.on('bot_off_finished', (data) => {
        botSwitch.disabled = false;
        if (data.success) {
            botSwitch.className = 'form-check-input custom-check';
            updateCurrentStatus({ current_activity: 'Offline' });
        } else {
            alert('Encountered error on shutdown')
        }
    });

    // ------------------ LOG EVENTS -----------------------------

    const logTxt = document.getElementById('console');

    const updateLog = (data) => {
        let buf = '';
        data.forEach((log) => {
            buf += `${log}\n`;
        });
        logTxt.value = buf;
        logTxt.scrollTop = logTxt.scrollHeight;
    };

    socket.on('bot_log_added', (data) => {
        updateLog(data.log);
    });
});
