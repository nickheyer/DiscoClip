document.addEventListener('DOMContentLoaded', (event) => {

    //On-Load Enviroment Settings
    setActiveTab("log-tab");
       
    function convertJson(str) { //Converts "pretty" string JSON to JSON object
        return JSON.parse(str);
    }

    function sendRequestWithData(url, data) { //Sends request with data to url, doesnt return
        const Http = new XMLHttpRequest();
        Http.open("POST", url);
        Http.setRequestHeader("Content-Type", "application/json");
        Http.send(JSON.stringify(data));
    }
    
    function sendNullRequest(url) { //Sends request with no data to url
        const Http = new XMLHttpRequest();
        Http.open("POST", url);
        Http.send();
    }
    
    function sendNullWithResponse(url) { //Sends request with no data to url, returns response sync
        const Http = new XMLHttpRequest();
        Http.open("POST", url);
        Http.send();
        Http.onreadystatechange = function () {
            if (this.readyState == this.DONE) {
                return this.responseText;
            }
        }
    }

    function postAndScrollDown(txt) { //Posts text to console object, if text is different then what is already being displayed
        const txtArea = document.getElementById('console');
        if (txt.trim() != txtArea.value.trim()) {
            txtArea.value = txt;
            txtArea.scrollTop = txtArea.scrollHeight;
        }
    };

    function switchTab(target) {
        document.getElementById(target).click();
    }

    function setActiveTab(tab) { //Sets active tab in localStorage
        localStorage.setItem("active-tab", tab);
        var con = document.getElementById('console');
        if (["Values-tab", "Request-tab"].includes(tab)) { //If tab is Values, allows for saving and editing of data
            saveButton.disabled = false;
            con.readOnly = false;

        }
        else {
            saveButton.disabled = true;
            con.readOnly  = true;
        }
        if (tab == "log-tab") { //If tab is log tab, posts log to screen
            getStat();
        }
        else {
            setDataToTab(tab);
        }
    };

    function getActiveTab() {
        var tab = localStorage.getItem("active-tab");
        return tab;
    };

    function setDataToTab(tab) {
        const tabData = document.getElementById(tab).value;
        const Http = new XMLHttpRequest();
        var url = "/data";
        Http.open("POST", url);
        Http.setRequestHeader("Content-Type", "application/json");
        Http.send(JSON.stringify({"document":tabData}));
        Http.onreadystatechange = function () {
            if (this.readyState == this.DONE) {
                localStorage.setItem(tab, JSON.stringify(JSON.parse(this.response), null, "\t"));
                postAndScrollDown(localStorage.getItem(tab));
            }
        }
    };

    function getStat() {
        if (getActiveTab() == "log-tab") {
        const Http = new XMLHttpRequest();
        var url = "/stat";
        Http.open("POST", url);
        Http.send();
        Http.onreadystatechange = function () {
            if (this.readyState == this.DONE) {
                 //If current tab is log tab, post data
                    postAndScrollDown(JSON.stringify(JSON.parse(this.response), null, "\t"));
                
                }
            }
        }
    };setInterval(getStat, 1000);


    var consoleTabs = document.getElementsByName("console-tabs")
    for (let x = 0; x < consoleTabs.length; x++) {
        consoleTabs[x].onclick = function() {         
            if (this.id != getActiveTab()) {
                setActiveTab(this.id);
            }       
        }
    }

    //Shutdown button listener
    var serverSwitch = document.getElementById("server-io-switch");
    $(serverSwitch).change(async function() {
        if (this.checked == false) {
            dIOSwitch.checked = false;
            sendNullRequest("/shutdown");
            postAndScrollDown("Goodbye!");
            await sleep(2000);
            location.reload();            
        }

    });

    //New on-off slider for discord bot
    var dIOSwitch = document.getElementById("discord-io-switch");
    $(dIOSwitch).change(async function() {
        if (this.checked) {
            const Http = new XMLHttpRequest();
            Http.open("POST", "/on");
            Http.setRequestHeader("Content-Type", "application/json");
            let data = JSON.stringify({"type":"discord"})
            Http.send(data);
            Http.onreadystatechange = function () {
                if (this.readyState == this.DONE) {
                    if (this.responseText == "Missing Values") {
                        dIOSwitch.checked = false;
                        alert("Error: Missing Values.\nCheck to see that all fields are filled.")
                        switchTab("Values-tab");
                    } 
                }
            }
            
        }
        else {
            sendRequestWithData("/off", {"type":"discord"});
        }
    });
    


    //Opening form tab
    document.getElementById("set-values-tab").onclick = function() {
        switchTab("Values-tab");
    }

    //Discord Save button listener
    document.getElementById("saveButton").onclick = function() {
        const txtAreaJson = JSON.parse(document.getElementById("console").value);
        var fileName = document.getElementById(getActiveTab()).value;
        var j = {"data":txtAreaJson, "file":fileName}
        sendRequestWithData("/save", j);
        switchTab("log-tab");
    }

    //Form Save button listener
    document.getElementById("cred-form-save").onclick = function() {
        const values = document.getElementsByName("cred-form-field");
        const txtAreaJson = JSON.parse(document.getElementById("console").value);

        for (let x = 0; x < values.length; x++) {
            let val = values[x].value;

            if (!isNaN(val)) {
                val = parseInt(val);
            } else if (['true', 'True'].includes(val)) {
                val = true;
            } else if (['false', 'False'].includes(val)) {
                val = false;
            }

            txtAreaJson[values[x].id] = val;
        }
        var j = {"data":txtAreaJson, "file":"values"};
        sendRequestWithData("/save", j);
        document.getElementById("close-form").click();
        switchTab("log-tab");
    }


    
});
